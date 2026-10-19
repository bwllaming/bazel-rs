//! C++ toolchain build variables and flag expansion (Bazel 9.3
//! `rules/cpp/CcToolchainVariables.java` and the expansion half of
//! `rules/cpp/CcToolchainFeatures.java`).
//!
//! - [`parse_chunks`] ports `StringValueParser` (CcToolchainVariables.java:140-253).
//! - [`CcVariables`] is the Starlark `Variables` value: `MapVariables`
//!   (:917-1012) and `SingleVariables` (:1014-1063) with a parent chain. Raw
//!   Starlark values are stored and converted on lookup, as `asVariableValue`
//!   (:826-840) does.
//! - [`Scope`] adds the iteration-only `SingleVariables` that `FlagGroup.expand`
//!   (CcToolchainFeatures.java:212-237) pushes for `iterate_over`.
//! - [`FlagGroup`], [`expand_flag_group`] and [`expand_env_entry`] port
//!   `Flag`, `FlagGroup.canBeExpanded` (:239-273) and `EnvEntry` (:156-188).
//!
//! Where Bazel 9.3 throws an unchecked Java exception (an internal crash rather
//! than a Starlark error), the native profile fails with an explicit error
//! naming the Bazel behaviour; see docs/CC_FEATURES.md.

use crate::language_modules::failure;
use allocative::Allocative;
use starlark::collections::StarlarkHasher;
use starlark::eval::Evaluator;
use starlark::starlark_complex_value;
use starlark::values::{
    Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Trace, Value,
    ValueLike, starlark_value,
};
use starlark::values::{FrozenHeap, FrozenHeapRef, FrozenValue};
use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::{Mutex, OnceLock};

/// `Invalid toolchain configuration: ` prefix shared by Bazel's expansion errors.
const INVALID: &str = "Invalid toolchain configuration";

// ---------------------------------------------------------------------------
// PathFragment (vfs/PathFragment.java, UnixOsPathPolicy.normalize and
// OsPathPolicy.Utils.removeRelativePaths).

/// Unix `PathFragment` normalization: `.` segments and repeated or trailing
/// slashes disappear, `..` removes a preceding non-`..` segment, and `..` at
/// the root of an absolute path is dropped. The empty fragment is `""`.
pub(crate) fn normalize_unix_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        match segment {
            "." => {}
            ".." if segments.last().is_some_and(|last| *last != "..") => {
                segments.pop();
            }
            ".." if absolute => {}
            _ => segments.push(segment),
        }
    }
    let joined = segments.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// `PathFragment.getSafePathString` (PathFragment.java:721-723).
pub(crate) fn safe_path_string(normalized: &str) -> String {
    if normalized.is_empty() {
        ".".to_owned()
    } else {
        normalized.to_owned()
    }
}

/// `PathFragment.getRelative` (PathFragment.java:254-262) of two normalized
/// fragments.
pub(crate) fn relative_path(base: &str, other: &str) -> String {
    if base.is_empty() || other.starts_with('/') {
        return other.to_owned();
    }
    if other.is_empty() {
        return base.to_owned();
    }
    normalize_unix_path(&format!("{base}/{other}"))
}

// ---------------------------------------------------------------------------
// StringValueParser.

/// One piece of a flag or environment value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Chunk {
    /// StringLiteralChunk.
    Literal(String),
    /// VariableChunk.
    Variable(String),
    /// RelativePathChunk: a normalized relative exec path (`%{path:...}`).
    Path(String),
}

/// `StringValueParser` (CcToolchainVariables.java:140-253). Positions in
/// errors are indices into Bazel's Java `String`, which holds the UTF-8
/// bytes as Latin-1 characters (Starlark strings are read as Latin-1,
/// `StarlarkUtil.createParserInput`), so they count UTF-8 bytes.
pub(crate) fn parse_chunks(value: &str) -> Result<Vec<Chunk>, String> {
    let bytes = value.as_bytes();
    let len = bytes.len();
    let position = |byte: usize| byte.min(len);
    let abort = |error: &str, at: usize| {
        format!(
            "{INVALID}: {error} at position {} while parsing a flag containing '{value}'",
            position(at)
        )
    };
    let mut chunks = Vec::new();
    let mut current = 0;
    while current < len {
        let at_variable =
            bytes[current] == b'%' && (current + 1 >= len || bytes[current + 1] != b'%');
        if at_variable {
            current += 1;
            if current >= len || bytes[current] != b'{' {
                return Err(abort("expected '{'", current));
            }
            current += 1;
            if current >= len || bytes[current] == b'}' {
                return Err(abort("expected variable name", current));
            }
            let Some(end) = value[current..].find('}').map(|offset| current + offset) else {
                // Bazel: value.substring(current, -1) throws
                // StringIndexOutOfBoundsException, an internal crash.
                return Err(format!(
                    "{INVALID}: unterminated variable reference at position {} while parsing a flag containing '{value}' (Bazel 9.3 fails with an internal StringIndexOutOfBoundsException)",
                    position(current)
                ));
            };
            let name = &value[current..end];
            if let Some(path) = name.strip_prefix("path:") {
                if path.is_empty() {
                    return Err(abort("expected path after 'path:'", current));
                }
                if path.starts_with('/') {
                    return Err(abort(
                        "expected relative Unix-style path after 'path:'",
                        current,
                    ));
                }
                chunks.push(Chunk::Path(normalize_unix_path(path)));
            } else {
                chunks.push(Chunk::Variable(name.to_owned()));
            }
            current = end + 1;
        } else {
            let mut start = current;
            if bytes[current] == b'%' {
                current += 1;
                start = current;
            }
            // Java value.indexOf('%', current + 1); '%' is never part of a
            // multi-byte UTF-8 sequence, so a byte search is exact.
            current = bytes[current + 1..]
                .iter()
                .position(|byte| *byte == b'%')
                .map_or(len, |offset| current + 1 + offset);
            chunks.push(Chunk::Literal(value[start..current].to_owned()));
        }
    }
    Ok(chunks)
}

// ---------------------------------------------------------------------------
// The Starlark `Variables` value.

// Only derive-generated marker/trace/coercion implementations need the
// exception; fields are owned strings plus traced Starlark values.
#[allow(unsafe_code)]
mod values {
    use super::*;

    /// MapVariables (keys sorted) or SingleVariables (one key). `parent` is
    /// another Variables value or None.
    #[derive(
        Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, Freeze, Coerce, StarlarkPagable,
    )]
    #[repr(C)]
    pub(crate) struct CcVariablesGen<V> {
        pub parent: V,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub single: bool,
        pub entries: Vec<(String, V)>,
        /// `structuredVariableCache` (CcToolchainVariables.java:292-297):
        /// per instance, never part of equality.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        #[allocative(skip)]
        #[starlark_pagable(skip)]
        pub lookup_cache: LookupCache,
    }
}
pub(crate) use values::CcVariablesGen;
starlark_complex_value!(pub(crate) CcVariables);

/// A `structuredVariableCache` entry: a found value (recomputed from the
/// immutable variables on a hit), Bazel's NULL_MARKER, or the cached
/// ExpansionException message (CcToolchainVariables.java:360-395).
#[derive(Clone, Debug)]
pub(crate) enum Cached {
    Found,
    NotFound,
    Error(String),
}

/// Dotted-name lookup results of one variables instance.
pub(crate) type LookupCache = Mutex<HashMap<String, Cached>>;

impl<V> fmt::Display for CcVariablesGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // No repr override: StarlarkValue's `<unknown object CLASS>`.
        write!(
            f,
            "<unknown object com.google.devtools.build.lib.rules.cpp.CcToolchainVariables${}>",
            if self.single {
                "SingleVariables"
            } else {
                "MapVariables"
            }
        )
    }
}

#[starlark_value(type = "Variables")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for CcVariablesGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    /// `MapVariables.equals` / `SingleVariables.equals`
    /// (CcToolchainVariables.java:993-1006, 1044-1057): same class, the same
    /// parent *instance*, equal keys and `Objects.equals` values.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        let Some(other) = CcVariables::from_value(other) else {
            return Ok(false);
        };
        let (parent, other_parent) = (self.parent.to_value(), other.parent);
        if self.single != other.single
            || !(parent.ptr_eq(other_parent) || parent.is_none() && other_parent.is_none())
            || self.entries.len() != other.entries.len()
        {
            return Ok(false);
        }
        for ((key, value), (other_key, other_value)) in self.entries.iter().zip(&other.entries) {
            if key != other_key || !value.to_value().equals(*other_value)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `isImmutable()` is true (MapVariables.java:952-955), so Variables are
    /// hashable. Hashing the class and keys is consistent with `equals`
    /// (Bazel's hashCode also mixes the values and the parent's identity).
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        "Variables".hash(hasher);
        self.single.hash(hasher);
        for (key, _) in &self.entries {
            key.hash(hasher);
        }
        Ok(())
    }
}

impl<'v, V: ValueLike<'v>> CcVariablesGen<V> {
    fn raw(&self, name: &str) -> Option<Value<'v>> {
        if self.single {
            return self
                .entries
                .first()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.to_value());
        }
        self.entries
            .binary_search_by(|(key, _)| key.as_str().cmp(name))
            .ok()
            .map(|index| self.entries[index].1.to_value())
    }

    fn parent(&self) -> Option<Value<'v>> {
        let parent = self.parent.to_value();
        (!parent.is_none()).then_some(parent)
    }
}

/// Whether `value` is a `Variables` value.
/// The values a Variables instance holds (its parent, then its entries), for
/// provider payload traversal.
pub(crate) fn payload<'v>(value: Value<'v>) -> Option<Vec<Value<'v>>> {
    let variables = CcVariables::from_value(value)?;
    let mut values = vec![variables.parent];
    values.extend(variables.entries.iter().map(|(_, value)| *value));
    Some(values)
}

pub(crate) fn is_variables(value: Value<'_>) -> bool {
    CcVariables::from_value(value).is_some()
}

/// LinkCommandLine.getParamCommandLine (LinkCommandLine.java:92-130): the
/// `linker_param_file` variable's value when it is available, whose flags are
/// filtered out of the parameter command line.
pub(crate) fn linker_param_file<'v>(
    variables: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<String>> {
    let scope = Scope::Root(variables);
    if !scope.is_available("linker_param_file", eval)? {
        return Ok(None);
    }
    Ok(Some(
        scope
            .get("linker_param_file", eval)?
            .string_value("linker_param_file")
            .map_err(crate::language_modules::failure)?,
    ))
}

/// `new MapVariables(parent, map)`: keys sorted (MapVariables.java:935-950).
pub(crate) fn alloc_map<'v>(
    eval: &Evaluator<'v, '_, '_>,
    parent: Option<Value<'v>>,
    mut entries: Vec<(String, Value<'v>)>,
) -> Value<'v> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    eval.heap().alloc(CcVariablesGen {
        parent: parent.unwrap_or_else(Value::new_none),
        single: false,
        entries,
        lookup_cache: LookupCache::default(),
    })
}

/// `cc_common.empty_variables()`: the `CcToolchainVariables.empty()`
/// singleton (CcToolchainVariables.java:273-290), an empty MapVariables
/// shared by every caller, so it is also the same parent instance.
pub(crate) fn empty<'v>(_eval: &Evaluator<'v, '_, '_>) -> Value<'v> {
    static EMPTY: OnceLock<(FrozenHeapRef, FrozenValue)> = OnceLock::new();
    let (_, value) = EMPTY.get_or_init(|| {
        let heap = FrozenHeap::new();
        let value = heap.alloc(FrozenCcVariables {
            parent: FrozenValue::new_none(),
            single: false,
            entries: Vec::new(),
            lookup_cache: LookupCache::default(),
        });
        (heap.into_ref(), value)
    });
    value.to_value()
}

/// `CcStarlarkInternal.combineCcToolchainVariables` (CcStarlarkInternal.java:146-168):
/// `Builder(parent).addAllNonTransitive(v)` for each, then `build()`.
pub(crate) fn combine<'v>(
    eval: &Evaluator<'v, '_, '_>,
    parent: Value<'v>,
    variables: &[Value<'v>],
) -> starlark::Result<Value<'v>> {
    let mut entries: Vec<(String, Value<'v>)> = Vec::new();
    for value in variables {
        let variables = CcVariables::from_value(*value).expect("type checked by the caller");
        let keys: Vec<&String> = variables.entries.iter().map(|(key, _)| key).collect();
        let intersection: Vec<&str> = keys
            .iter()
            .filter(|key| entries.iter().any(|(existing, _)| existing == **key))
            .map(|key| key.as_str())
            .collect();
        if !intersection.is_empty() {
            // Preconditions.checkArgument: an IllegalArgumentException, which
            // Bazel 9.3 does not convert into a Starlark error.
            return Err(failure(format!(
                "Cannot overwrite existing variables: [{}] (Bazel 9.3 fails with an internal IllegalArgumentException)",
                intersection.join(", ")
            )));
        }
        entries.extend(
            variables
                .entries
                .iter()
                .map(|(key, value)| (key.clone(), *value)),
        );
    }
    if entries.len() == 1 {
        return Ok(eval.heap().alloc(CcVariablesGen {
            parent,
            single: true,
            entries,
            lookup_cache: LookupCache::default(),
        }));
    }
    Ok(alloc_map(eval, Some(parent), entries))
}

// ---------------------------------------------------------------------------
// Variable values (CcToolchainVariables.VariableValue and implementations).

/// A converted variable value.
#[derive(Clone, Debug)]
pub(crate) enum VarValue<'v> {
    /// StringValue (also a Label's canonical form).
    String(String),
    /// BooleanValue.
    Boolean(bool),
    /// ArtifactValue: the exec path.
    Artifact(String),
    /// Sequence of raw elements, converted lazily.
    Sequence(Vec<Value<'v>>),
    /// StarlarkStructureAdapter.
    Structure(Value<'v>),
}

impl<'v> VarValue<'v> {
    fn type_name(&self) -> String {
        match self {
            VarValue::String(_) => "string".to_owned(),
            VarValue::Boolean(_) => "boolean".to_owned(),
            VarValue::Artifact(_) => "artifact".to_owned(),
            VarValue::Sequence(_) => "sequence".to_owned(),
            VarValue::Structure(value) => value.get_type().to_owned(),
        }
    }

    fn is_truthy(&self) -> bool {
        match self {
            VarValue::String(value) => !value.is_empty(),
            VarValue::Boolean(value) => *value,
            VarValue::Artifact(_) => true,
            VarValue::Sequence(values) => !values.is_empty(),
            // StructImpl does not override StarlarkValue.truth().
            VarValue::Structure(_) => true,
        }
    }

    /// `getStringValue` (VariableValueAdapter.java:555-563 and overrides).
    fn string_value(&self, variable: &str) -> Result<String, String> {
        match self {
            VarValue::String(value) | VarValue::Artifact(value) => Ok(value.clone()),
            VarValue::Boolean(value) => Ok(if *value { "1" } else { "0" }.to_owned()),
            VarValue::Sequence(_) | VarValue::Structure(_) => Err(format!(
                "{INVALID}: Cannot expand variable '{variable}': expected string, found {}",
                self.type_name()
            )),
        }
    }
}

fn is_iterable(value: Value<'_>) -> bool {
    matches!(
        value.get_type(),
        "list" | "tuple" | "dict" | "set" | "range"
    )
}

fn iterate<'v>(value: Value<'v>, eval: &Evaluator<'v, '_, '_>) -> starlark::Result<Vec<Value<'v>>> {
    Ok(value.iterate(eval.heap())?.collect())
}

/// `asVariableValue` (CcToolchainVariables.java:826-840). `Ok(None)` is Java
/// null (None). Values of other types make Bazel throw ClassCastException.
pub(crate) fn as_variable_value<'v>(
    raw: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<VarValue<'v>>> {
    if raw.is_none() {
        return Ok(None);
    }
    if let Some(value) = raw.unpack_bool() {
        return Ok(Some(VarValue::Boolean(value)));
    }
    if let Some(value) = raw.unpack_str() {
        return Ok(Some(VarValue::String(value.to_owned())));
    }
    if let Some(label) = crate::starlark_types::label_canonical_form(raw) {
        return Ok(Some(VarValue::String(label)));
    }
    if raw.get_type() == "File" {
        let file = crate::starlark_types::unpack_file(raw).map_err(starlark::Error::new_other)?;
        return Ok(Some(VarValue::Artifact(file.path)));
    }
    if is_iterable(raw) {
        return Ok(Some(VarValue::Sequence(iterate(raw, eval)?)));
    }
    if raw.get_type() == "depset" {
        let to_list = raw.get_attr_error("to_list", eval.heap())?;
        let list = eval.eval_function(to_list, &[], &[])?;
        return Ok(Some(VarValue::Sequence(iterate(list, eval)?)));
    }
    if raw.get_type() == "struct" {
        return Ok(Some(VarValue::Structure(raw)));
    }
    Err(failure(format!(
        "{INVALID}: a value of type '{}' cannot be used as a C++ toolchain variable (Bazel 9.3 fails with an internal ClassCastException)",
        raw.get_type()
    )))
}

/// `getSequenceValue` (CcToolchainVariables.java:572-582).
fn sequence_value<'v>(variable: &str, value: &VarValue<'v>) -> Result<Vec<Value<'v>>, String> {
    match value {
        VarValue::Sequence(values) => Ok(values.clone()),
        other => Err(format!(
            "{INVALID}: Cannot expand variable '{variable}': expected sequence, found {}",
            other.type_name()
        )),
    }
}

/// `getFieldValue` (VariableValueAdapter.java:535-553 and
/// StarlarkStructureAdapter.java:855-904).
/// The outer error is a Bazel crash or Starlark failure; the inner one an
/// ExpansionException message.
fn field_value<'v>(
    value: &VarValue<'v>,
    variable: &str,
    field: &str,
    throw: bool,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Result<Option<VarValue<'v>>, String>> {
    let VarValue::Structure(structure) = value else {
        if throw {
            return Ok(Err(format!(
                "{INVALID}: Cannot expand variable '{variable}.{field}': variable '{variable}' is {}, expected structure",
                value.type_name()
            )));
        }
        return Ok(Ok(None));
    };
    let Some(field_value) = structure.get_attr(field, eval.heap())? else {
        return Ok(Ok(None));
    };
    if is_iterable(field_value) {
        return Ok(Ok(Some(VarValue::Sequence(iterate(field_value, eval)?))));
    }
    as_variable_value(field_value, eval).map(Ok)
}

// ---------------------------------------------------------------------------
// Lookup.

/// The variables visible during one expansion: a `Variables` value, or an
/// `iterate_over` binding on top of an outer scope.
pub(crate) enum Scope<'a, 'v> {
    Root(Value<'v>),
    /// `new SingleVariables(parent, name, value)` (CcToolchainFeatures.java:226-227),
    /// a fresh instance (with a fresh lookup cache) per element.
    Iteration {
        parent: &'a Scope<'a, 'v>,
        name: &'a str,
        /// None is a Java null sequence element (a None in the list).
        value: Option<VarValue<'v>>,
        cache: LookupCache,
    },
}

impl<'v> Scope<'_, 'v> {
    fn cache(&self) -> &LookupCache {
        match self {
            Scope::Root(variables) => {
                &CcVariables::from_value(*variables)
                    .expect("Variables root")
                    .lookup_cache
            }
            Scope::Iteration { cache, .. } => cache,
        }
    }

    /// `getNonStructuredVariable`.
    fn non_structured(
        &self,
        name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Option<VarValue<'v>>> {
        match self {
            Scope::Root(variables) => {
                let mut current = Some(*variables);
                while let Some(value) = current {
                    let variables = CcVariables::from_value(value).expect("Variables chain");
                    if let Some(raw) = variables.raw(name) {
                        // MapVariables/SingleVariables: a present key is never
                        // looked up in the parent, even when its value is None.
                        return as_variable_value(raw, eval);
                    }
                    current = variables.parent();
                }
                Ok(None)
            }
            Scope::Iteration {
                parent,
                name: bound,
                value,
                ..
            } => {
                if *bound != name {
                    return parent.non_structured(name, eval);
                }
                match value {
                    Some(value) => Ok(Some(value.clone())),
                    // asVariableValue(null): the pattern switch throws
                    // NullPointerException.
                    None => Err(failure(format!(
                        "{INVALID}: element of sequence variable '{name}' is None (Bazel 9.3 fails with an internal NullPointerException)"
                    ))),
                }
            }
        }
    }

    /// `lookupVariable` (CcToolchainVariables.java:340-396), including the
    /// per-instance `structuredVariableCache` of dotted names: the first
    /// lookup of a dotted name decides the cached outcome, so a failed
    /// availability check makes a later expansion report the generic
    /// `Cannot find variable named` text.
    fn lookup(
        &self,
        name: &str,
        throw: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Option<VarValue<'v>>> {
        if let Some(value) = self.non_structured(name, eval)? {
            return Ok(Some(value));
        }
        let missing = || failure(format!("{INVALID}: Cannot find variable named '{name}'."));
        if !name.contains('.') {
            return if throw { Err(missing()) } else { Ok(None) };
        }
        let cached = self
            .cache()
            .lock()
            .expect("lookup cache")
            .get(name)
            .cloned();
        let outcome = match cached {
            Some(outcome) => outcome,
            None => {
                // Bazel crashes (unchecked exceptions) are not cached.
                let computed = self.structure_variable(name, throw, eval)?;
                let outcome = match &computed {
                    Ok(Some(_)) => Cached::Found,
                    Ok(None) => Cached::NotFound,
                    Err(message) => Cached::Error(message.clone()),
                };
                self.cache()
                    .lock()
                    .expect("lookup cache")
                    .entry(name.to_owned())
                    .or_insert(outcome.clone());
                if let Ok(Some(value)) = computed {
                    return Ok(Some(value));
                }
                outcome
            }
        };
        match outcome {
            Cached::Found => Ok(self
                .structure_variable(name, false, eval)?
                .unwrap_or_default()),
            Cached::NotFound if throw => Err(missing()),
            Cached::Error(message) if throw => Err(failure(message)),
            Cached::NotFound | Cached::Error(_) => Ok(None),
        }
    }

    /// `getStructureVariable` (CcToolchainVariables.java:398-441). The inner
    /// error is an ExpansionException message.
    fn structure_variable(
        &self,
        name: &str,
        throw: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Result<Option<VarValue<'v>>, String>> {
        let mut fields: Vec<&str> = Vec::new();
        let mut path = name;
        let mut variable;
        loop {
            let dot = path.rfind('.').expect("path contains '.'");
            fields.push(&path[dot + 1..]);
            path = &path[..dot];
            variable = self.non_structured(path, eval)?;
            if variable.is_some() || !path.contains('.') {
                break;
            }
        }
        let Some(mut variable) = variable else {
            return Ok(Ok(None));
        };
        while let Some(field) = fields.pop() {
            // Bazel passes the unchanged structPath for every nested field.
            match field_value(&variable, path, field, throw, eval)? {
                Ok(Some(next)) => variable = next,
                Ok(None) if throw => {
                    return Ok(Err(format!(
                        "{INVALID}: Cannot expand variable '{path}.{field}': structure {path} doesn't have a field named '{field}'"
                    )));
                }
                Ok(None) => return Ok(Ok(None)),
                Err(message) => return Ok(Err(message)),
            }
        }
        Ok(Ok(Some(variable)))
    }

    pub(crate) fn get(
        &self,
        name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<VarValue<'v>> {
        Ok(self.lookup(name, true, eval)?.expect("throwing lookup"))
    }

    pub(crate) fn is_available(
        &self,
        name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        Ok(self.lookup(name, false, eval)?.is_some())
    }

    fn string_variable(
        &self,
        name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        self.get(name, eval)?.string_value(name).map_err(failure)
    }
}

fn expand_chunks<'v>(
    chunks: &[Chunk],
    scope: &Scope<'_, 'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<String> {
    let mut text = String::new();
    for chunk in chunks {
        match chunk {
            Chunk::Literal(literal) => text.push_str(literal),
            Chunk::Path(path) => text.push_str(path),
            Chunk::Variable(name) => text.push_str(&scope.string_variable(name, eval)?),
        }
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// Flag groups and environment entries.

/// A flag or a nested flag group (CcToolchainVariables.Expandable).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Expandable {
    Flag(Vec<Chunk>),
    Group(FlagGroup),
}

/// CcToolchainFeatures.FlagGroup (CcToolchainFeatures.java:201-298).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FlagGroup {
    pub expandables: Vec<Expandable>,
    pub iterate_over: Option<String>,
    pub expand_if_all_available: Vec<String>,
    pub expand_if_none_available: Vec<String>,
    pub expand_if_true: Option<String>,
    pub expand_if_false: Option<String>,
    pub expand_if_equal: Option<(String, String)>,
}

/// `FlagGroup.canBeExpanded` (CcToolchainFeatures.java:239-273).
fn can_be_expanded<'v>(
    group: &FlagGroup,
    scope: &Scope<'_, 'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<bool> {
    for variable in &group.expand_if_all_available {
        if !scope.is_available(variable, eval)? {
            return Ok(false);
        }
    }
    for variable in &group.expand_if_none_available {
        if scope.is_available(variable, eval)? {
            return Ok(false);
        }
    }
    if let Some(variable) = &group.expand_if_true
        && (!scope.is_available(variable, eval)? || !scope.get(variable, eval)?.is_truthy())
    {
        return Ok(false);
    }
    if let Some(variable) = &group.expand_if_false
        && (!scope.is_available(variable, eval)? || scope.get(variable, eval)?.is_truthy())
    {
        return Ok(false);
    }
    if let Some((variable, expected)) = &group.expand_if_equal {
        if !scope.is_available(variable, eval)? {
            return Ok(false);
        }
        let actual = scope
            .get(variable, eval)?
            .string_value(variable)
            .map_err(failure)?;
        if &actual != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `FlagGroup.expand` (CcToolchainFeatures.java:212-237).
pub(crate) fn expand_flag_group<'v>(
    group: &FlagGroup,
    scope: &Scope<'_, 'v>,
    eval: &mut Evaluator<'v, '_, '_>,
    command_line: &mut Vec<String>,
) -> starlark::Result<()> {
    if !can_be_expanded(group, scope, eval)? {
        return Ok(());
    }
    if let Some(variable) = &group.iterate_over {
        let value = scope.get(variable, eval)?;
        for raw in sequence_value(variable, &value).map_err(failure)? {
            let element = as_variable_value(raw, eval)?;
            let nested = Scope::Iteration {
                parent: scope,
                name: variable,
                value: element,
                cache: LookupCache::default(),
            };
            for expandable in &group.expandables {
                expand(expandable, &nested, eval, command_line)?;
            }
        }
    } else {
        for expandable in &group.expandables {
            expand(expandable, scope, eval, command_line)?;
        }
    }
    Ok(())
}

fn expand<'v>(
    expandable: &Expandable,
    scope: &Scope<'_, 'v>,
    eval: &mut Evaluator<'v, '_, '_>,
    command_line: &mut Vec<String>,
) -> starlark::Result<()> {
    match expandable {
        Expandable::Flag(chunks) => {
            command_line.push(expand_chunks(chunks, scope, eval)?);
            Ok(())
        }
        Expandable::Group(group) => expand_flag_group(group, scope, eval, command_line),
    }
}

/// CcToolchainFeatures.EnvEntry (CcToolchainFeatures.java:156-188).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct EnvEntry {
    pub key: String,
    pub value: Vec<Chunk>,
    pub expand_if_all_available: Vec<String>,
}

/// `EnvEntry.addEnvEntry`: `None` when an `expand_if_available` variable is
/// missing.
pub(crate) fn expand_env_entry<'v>(
    entry: &EnvEntry,
    scope: &Scope<'_, 'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<String>> {
    for variable in &entry.expand_if_all_available {
        if !scope.is_available(variable, eval)? {
            return Ok(None);
        }
    }
    expand_chunks(&entry.value, scope, eval).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_matches_string_value_parser() {
        use Chunk::*;
        let lit = |s: &str| Literal(s.to_owned());
        let var = |s: &str| Variable(s.to_owned());
        assert_eq!(parse_chunks("").unwrap(), vec![]);
        assert_eq!(
            parse_chunks("-f %{a}/%{b.c}").unwrap(),
            vec![lit("-f "), var("a"), lit("/"), var("b.c")]
        );
        // "abc%%def" is two literal chunks "abc" and "%def".
        assert_eq!(
            parse_chunks("abc%%def").unwrap(),
            vec![lit("abc"), lit("%def")]
        );
        assert_eq!(
            parse_chunks("%%%{x}%%").unwrap(),
            vec![lit("%"), var("x"), lit("%")]
        );
        assert_eq!(parse_chunks("%%%%").unwrap(), vec![lit("%"), lit("%")]);
        assert_eq!(
            parse_chunks("%{path:a/./b/../c}").unwrap(),
            vec![Path("a/c".to_owned())]
        );
        for (input, message) in [
            (
                "%",
                "expected '{' at position 1 while parsing a flag containing '%'",
            ),
            (
                "%x",
                "expected '{' at position 1 while parsing a flag containing '%x'",
            ),
            (
                "a%{}",
                "expected variable name at position 3 while parsing a flag containing 'a%{}'",
            ),
            ("%{", "expected variable name at position 2"),
            ("%{path:}", "expected path after 'path:' at position 2"),
            (
                "%{path:/abs}",
                "expected relative Unix-style path after 'path:' at position 2",
            ),
            (
                "é%x",
                "expected '{' at position 3 while parsing a flag containing 'é%x'",
            ),
            ("😀%x", "expected '{' at position 5"),
            ("%{abc", "unterminated variable reference at position 2"),
        ] {
            let error = parse_chunks(input).unwrap_err();
            assert!(error.contains(message), "{input}: {error}");
            assert!(error.starts_with("Invalid toolchain configuration: "));
        }
    }

    #[test]
    fn path_fragment_normalization() {
        for (input, expected) in [
            ("", ""),
            (".", ""),
            ("a//b/", "a/b"),
            ("a/../..", ".."),
            ("../a", "../a"),
            ("/..", "/"),
            ("/a/./b/../c", "/a/c"),
            ("//", "/"),
        ] {
            assert_eq!(normalize_unix_path(input), expected, "{input}");
        }
        assert_eq!(relative_path("tools/cc", "bin/gcc"), "tools/cc/bin/gcc");
        assert_eq!(relative_path("tools/cc", "../gcc"), "tools/gcc");
        assert_eq!(relative_path("tools/cc", "/usr/bin/gcc"), "/usr/bin/gcc");
        assert_eq!(relative_path("", "gcc"), "gcc");
        assert_eq!(safe_path_string(""), ".");
    }
}
