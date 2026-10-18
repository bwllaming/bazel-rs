//! Depset elements beyond the owned scalar kinds (N49): immutable `struct()`
//! values and Starlark provider instances, and the immutable values they may
//! hold (None, depsets, configured targets, runfiles, global functions).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/`.
//!
//! Native depsets are owned Rust data (a shared DAG, crate::starlark_types),
//! so an element is **snapshotted** when the depset is created and allocated
//! anew by `to_list()`: Bazel keeps the element object, but every element
//! Bazel accepts is immutable (Depset.checkElement with the strict
//! `--incompatible_always_check_depset_elements`,
//! com/google/devtools/build/lib/collect/nestedset/Depset.java:125-152), so a
//! snapshot is observably the same value: equal (StructImpl.equals,
//! com/google/devtools/build/lib/packages/StructImpl.java:97-132), with the
//! same fields, repr and type. Values whose identity Bazel compares (depsets
//! by their NestedSet, targets, runfiles, functions) keep it through shared
//! native handles. N49b: a frozen list or dict inside an element is an owned
//! snapshot too (StarlarkList.equals element-wise and never equal to a tuple,
//! net/starlark/java/eval/StarlarkList.java:244-252; Dict by contents,
//! Dict.java:203-211) and is re-allocated **frozen** by `to_list()`
//! ([`alloc_frozen_container`]). A value that is immutable in Bazel but has no
//! owned native form (a frozen set, a closure, other built-in objects) fails
//! explicitly.
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use allocative::Allocative;
use anyhow::bail;
use starlark::collections::SmallMap;
use starlark::environment::Module;
use starlark::values::dict::{Dict, DictRef};
use starlark::values::list::{AllocList, ListRef};
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;
use starlark::values::{Heap, OwnedFrozenValue, Value};

use crate::starlark_types::{Depset, File, Item, SymlinkEntry, Target};

/// Nesting bound of snapshotted values (tuples and structs).
const MAX_DEPTH: usize = 128;

/// An immutable struct or provider instance (StarlarkInfo).
#[derive(Clone, Debug, Allocative)]
pub(crate) struct StructItem {
    /// `None` for `struct()` (StructProvider.STRUCT); the provider key and
    /// display name of a Starlark provider instance.
    pub provider: Option<(String, String)>,
    /// Sorted by field name (StarlarkInfo tables are sorted by key).
    pub fields: Vec<(String, Item)>,
    /// Info.getCreationLocation of a provider instance; never compared.
    #[allocative(skip)]
    pub location: Option<starlark::codemap::FileSpan>,
}

impl PartialEq for StructItem {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider && self.fields == other.fields
    }
}

impl Eq for StructItem {}

impl PartialOrd for StructItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StructItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.provider, &self.fields).cmp(&(&other.provider, &other.fields))
    }
}

/// StructImpl.repr (packages/StructImpl.java:139-153): `struct(` and the
/// fields sorted by name as `name = repr`.
impl fmt::Display for StructItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("struct(")?;
        for (index, (name, value)) in self.fields.iter().enumerate() {
            if index != 0 {
                f.write_str(", ")?;
            }
            write!(f, "{name} = {value}")?;
        }
        f.write_str(")")
    }
}

impl StructItem {
    pub(crate) fn alloc<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        let fields = self
            .fields
            .iter()
            .map(|(name, value)| (name.clone(), value.alloc(heap)))
            .collect::<Vec<_>>();
        match &self.provider {
            None => heap.alloc(starlark::values::structs::AllocStruct(fields)),
            Some((identity, name)) => crate::providers::alloc_instance(
                heap,
                identity.clone(),
                name.clone(),
                fields,
                self.location.clone(),
            ),
        }
    }
}

/// A frozen dict inside a depset element (N49b), in insertion order.
/// Dict.equals/hashCode compare the contents
/// (net/starlark/java/eval/Dict.java:203-211: `contents.equals`, a
/// LinkedHashMap, so order-insensitive); equality and ordering here use the
/// entries sorted by key (keys are unique).
#[derive(Clone, Debug, Allocative)]
pub(crate) struct DictItem(pub Vec<(Item, Item)>);

impl DictItem {
    fn sorted(&self) -> Vec<&(Item, Item)> {
        let mut entries = self.0.iter().collect::<Vec<_>>();
        entries.sort();
        entries
    }
}

impl PartialEq for DictItem {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len() && self.sorted() == other.sorted()
    }
}

impl Eq for DictItem {}

impl PartialOrd for DictItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DictItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sorted().cmp(&other.sorted())
    }
}

/// A frozen list or dict re-allocated from its snapshot (N49b): on a temporary
/// module's heap, frozen with it, and kept alive by `heap` — the
/// crate::ctx_surface `frozen` pattern (a value on a private frozen heap the
/// receiving heap references), through a module freeze because struct and
/// provider-instance contents have no direct frozen-heap constructor. Mutating
/// it fails with Bazel's `trying to mutate a frozen list value` (`dict`)
/// (Mutability.checkMutable, net/starlark/java/eval/Starlark.java:264).
pub(crate) fn alloc_frozen_container<'v>(heap: Heap<'v>, item: &Item) -> Value<'v> {
    let owned = Module::with_temp_heap(|module| {
        let h = module.heap();
        let value = match item {
            Item::List(items) => h.alloc(AllocList(items.iter().map(|item| item.alloc(h)))),
            Item::Dict(dict) => {
                let mut map = SmallMap::with_capacity(dict.0.len());
                for (key, value) in &dict.0 {
                    let key = key.alloc(h);
                    // The snapshot of a key of a (hashable) dict re-allocates
                    // a value of the same type and contents.
                    let key = key
                        .get_hashed()
                        .expect("a snapshot of a dict key stays hashable");
                    map.insert_hashed(key, value.alloc(h));
                }
                h.alloc(Dict::new(map))
            }
            _ => item.alloc(h),
        };
        module.set("value", value);
        module
            .freeze()
            .expect("freezing owned depset element values cannot fail")
            .get("value")
            .expect("the frozen module has the value")
    });
    heap.access_owned_frozen_value(&owned)
}

/// A depset as a value: Depset.equals delegates to NestedSet's identity
/// (collect/nestedset/Depset.java:552-562; NestedSet has no equals
/// override), so a depset equals only itself — and every empty depset of
/// an order is the order's shared empty set (Order.emptyDepset). Paired
/// capture O/reports: `depset([1]) == depset([1])` is False. Natively the
/// identity is the shared DAG node.
#[derive(Clone, Debug, Allocative)]
pub(crate) struct DepsetItem(pub Depset);

impl DepsetItem {
    pub(crate) fn key(&self) -> (&'static str, usize) {
        let node = &self.0.node;
        if node.empty {
            (node.order.name(), 0)
        } else {
            (node.order.name(), Arc::as_ptr(node) as usize)
        }
    }
}

impl PartialEq for DepsetItem {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for DepsetItem {}

impl PartialOrd for DepsetItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DepsetItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

fn identity_hash(value: Value<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.identity().hash(&mut hasher);
    hasher.finish()
}

/// A configured target, compared by identity (AbstractConfiguredTarget has
/// no equals override): the one `Arc<Target>` per ConfiguredTargetKey for
/// the whole analysis (N55, crate::engine `ActionPlanning`), so a snapshot
/// taken by one target and a snapshot taken by another — and each
/// consumer's own `ctx.attr` value — are all the same instance.
#[derive(Clone, Debug, Allocative)]
pub(crate) struct TargetItem(#[allocative(skip)] pub Arc<Target>);

impl TargetItem {
    fn key(&self) -> (String, Vec<(&str, u64)>) {
        (
            self.0.label.canonical_form(),
            self.0
                .providers
                .iter()
                .map(|(key, value)| (key.as_str(), identity_hash(value.value())))
                .collect(),
        )
    }
}

impl PartialEq for TargetItem {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for TargetItem {}

impl PartialOrd for TargetItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TargetItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self == other {
            return std::cmp::Ordering::Equal;
        }
        self.key().cmp(&other.key())
    }
}

/// A runfiles object, compared by identity (crate::runfiles::Runfiles::identity_key).
#[derive(Clone, Debug, Allocative)]
pub(crate) struct RunfilesItem(#[allocative(skip)] pub Arc<crate::runfiles::Runfiles>);

impl PartialEq for RunfilesItem {
    fn eq(&self, other: &Self) -> bool {
        self.0.identity_key() == other.0.identity_key()
    }
}

impl Eq for RunfilesItem {}

impl PartialOrd for RunfilesItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RunfilesItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.identity_key().cmp(&other.0.identity_key())
    }
}

/// A global `def` of a loaded `.bzl` file (StarlarkFunction.equals compares
/// identity tokens, net/starlark/java/eval/StarlarkFunction.java:255-270).
#[derive(Clone, Debug, Allocative)]
pub(crate) struct FunctionItem(#[allocative(skip)] pub OwnedFrozenValue);

impl PartialEq for FunctionItem {
    fn eq(&self, other: &Self) -> bool {
        self.0.value().ptr_eq(other.0.value())
    }
}

impl Eq for FunctionItem {}

impl PartialOrd for FunctionItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FunctionItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self == other {
            return std::cmp::Ordering::Equal;
        }
        (self.0.value().to_repr(), identity_hash(self.0.value()))
            .cmp(&(other.0.value().to_repr(), identity_hash(other.0.value())))
    }
}

/// Starlark.isImmutable of a value, as far as the native profile can tell.
enum Mutability {
    Immutable,
    Mutable,
    /// A value class whose isImmutable the native profile does not model.
    Unknown(String),
}

fn is_starlark_def(value: Value<'_>) -> bool {
    let repr = value.to_repr();
    value.get_type() == "function" && repr.starts_with("<function ") && repr.contains(" from ")
}

/// Starlark.isImmutable (net/starlark/java/eval/Starlark.java:156-167) and
/// the classes' isImmutable overrides: String/Boolean/StarlarkInt/NoneType,
/// Label, Artifact, SymlinkEntry, Depset, configured targets, Runfiles and
/// StarlarkFunction are immutable; a Tuple when its elements are
/// (Tuple.java); StarlarkList/Dict/StarlarkSet when frozen; a StarlarkInfo
/// (struct or provider instance) when its provider is exported and every
/// field value is immutable (StarlarkInfoNoSchema.java:196-208,
/// StarlarkInfoWithSchema.java:141-155); a BuiltinFunction never
/// (StarlarkValue.isImmutable default).
fn mutability(value: Value<'_>, depth: usize) -> Mutability {
    use Mutability::*;
    if depth > MAX_DEPTH {
        return Unknown("value nested deeper than 128".to_owned());
    }
    let all = |values: &mut dyn Iterator<Item = Value<'_>>| -> Mutability {
        let mut result = Immutable;
        for value in values {
            match mutability(value, depth + 1) {
                Mutable => return Mutable,
                Unknown(kind) => result = Unknown(kind),
                Immutable => {}
            }
        }
        result
    };
    if value.is_none()
        || value.unpack_bool().is_some()
        || value.unpack_str().is_some()
        || value.get_type() == "int"
        || File::from_value(value).is_some()
        || SymlinkEntry::from_value(value).is_some()
        || crate::starlark_types::unpack_label(value).is_some()
        || Depset::from_value(value).is_some()
        || crate::starlark_types::TargetValue::from_value(value).is_some()
        || crate::runfiles::Runfiles::from_value(value).is_some()
        || is_starlark_def(value)
    {
        return Immutable;
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return all(&mut tuple.iter());
    }
    if matches!(value.get_type(), "list" | "dict" | "set") {
        return if value.unpack_frozen().is_some() {
            Immutable
        } else {
            Mutable
        };
    }
    if let Some(structure) = StructRef::from_value(value) {
        return all(&mut structure.iter().map(|(_, value)| value));
    }
    if let Some((_, _, fields, _)) = crate::providers::instance_parts(value) {
        // Native instances exist only for exported providers.
        return all(&mut fields.into_iter().map(|(_, value)| value));
    }
    if matches!(value.get_type(), "builtin_function_or_method") || {
        let repr = value.to_repr();
        value.get_type() == "function" && repr.starts_with("<built-in ")
    } {
        return Mutable;
    }
    Unknown(value.get_type().to_owned())
}

/// Depset.checkElement (collect/nestedset/Depset.java:125-152) in the strict
/// mode (`--incompatible_always_check_depset_elements`, default true): a
/// mutable value, then a top-level list or dict.
pub(crate) fn check_element(value: Value<'_>) -> anyhow::Result<()> {
    match mutability(value, 0) {
        Mutability::Mutable => bail!("depset elements must not be mutable values"),
        Mutability::Unknown(kind) => bail!(
            "a depset element containing a value of type '{kind}' is not implemented in the native profile (its immutability is not modelled)"
        ),
        Mutability::Immutable => {}
    }
    if ListRef::from_value(value).is_some() || value.get_type() == "dict" {
        bail!(
            "depsets cannot contain items of type '{}'",
            value.get_type()
        );
    }
    Ok(())
}

/// The frozen copy of a list-like or dict (`elements` are the elements, or
/// the key/value pairs in turn): `cc_internal.freeze`'s
/// StarlarkList/Dict.immutableCopyOf (N49b, crate::cc_common::freeze),
/// re-allocated from owned snapshots. Bazel's copy is shallow; natively every
/// element must be immutable (then shallow and deep agree) and have an owned
/// snapshot that keeps its identity: a configured target's shared `Arc`
/// (N55) re-allocates as the same instance. Otherwise the error names the
/// offending element.
pub(crate) fn frozen_copy<'v>(
    heap: Heap<'v>,
    elements: &[Value<'v>],
    pairs: bool,
    functions: &dyn Fn(Value<'_>) -> Option<OwnedFrozenValue>,
) -> Result<Value<'v>, String> {
    let mut items = Vec::with_capacity(elements.len());
    for &value in elements {
        let kind = value.get_type();
        match mutability(value, 0) {
            Mutability::Immutable => {}
            Mutability::Mutable => {
                return Err(format!(
                    "a mutable value of type '{kind}' (Bazel's shallow immutable copy keeps it mutable)"
                ));
            }
            Mutability::Unknown(inner) => {
                return Err(format!(
                    "a value of type '{kind}' holding a value of type '{inner}' (its immutability is not modelled)"
                ));
            }
        }
        let item = snapshot(value, functions, 0).map_err(|error| {
            let error = error.to_string();
            match error
                .strip_prefix("a depset element containing ")
                .and_then(|what| what.strip_suffix(" is not implemented in the native profile"))
            {
                Some(what) if kind == "struct" || kind == "tuple" => {
                    format!("a value of type '{kind}' holding {what}")
                }
                Some(what) => what.to_owned(),
                None => format!("a value of type '{kind}' ({error})"),
            }
        })?;
        items.push(item);
    }
    let container = if pairs {
        let mut entries = Vec::with_capacity(items.len() / 2);
        let mut items = items.into_iter();
        while let (Some(key), Some(value)) = (items.next(), items.next()) {
            entries.push((key, value));
        }
        Item::Dict(DictItem(entries))
    } else {
        Item::List(items)
    };
    Ok(alloc_frozen_container(heap, &container))
}

/// The owned snapshot of a checked (immutable) element.
pub(crate) fn snapshot(
    value: Value<'_>,
    functions: &dyn Fn(Value<'_>) -> Option<OwnedFrozenValue>,
    depth: usize,
) -> anyhow::Result<Item> {
    if depth > MAX_DEPTH {
        bail!("depset element exceeds nesting depth limit {MAX_DEPTH}");
    }
    let gap = |what: &str| -> anyhow::Result<Item> {
        bail!("a depset element containing {what} is not implemented in the native profile")
    };
    Ok(if let Some(file) = File::from_value(value) {
        Item::File(file.clone())
    } else if let Some(entry) = SymlinkEntry::from_value(value) {
        Item::Symlink(entry.clone())
    } else if let Some(text) = value.unpack_str() {
        Item::String(text.to_owned())
    } else if let Some(label) = crate::starlark_types::label_item(value) {
        label
    } else if let Some(flag) = value.unpack_bool() {
        Item::Bool(flag)
    } else if let Some(number) = value.unpack_i32() {
        Item::Int(number)
    } else if value.get_type() == "int" {
        bail!("depset integers outside the signed 32-bit range are unsupported");
    } else if value.is_none() {
        Item::None
    } else if let Some(tuple) = TupleRef::from_value(value) {
        Item::Tuple(
            tuple
                .iter()
                .map(|item| snapshot(item, functions, depth + 1))
                .collect::<anyhow::Result<_>>()?,
        )
    } else if let Some(depset) = Depset::from_value(value) {
        Item::Depset(DepsetItem(depset.clone()))
    } else if let Some(target) = crate::starlark_types::TargetValue::from_value(value) {
        Item::Target(TargetItem(target.0.clone()))
    } else if let Some(runfiles) = crate::runfiles::Runfiles::from_value(value) {
        Item::Runfiles(RunfilesItem(Arc::new(runfiles.clone())))
    } else if is_starlark_def(value) {
        match functions(value) {
            Some(function) => Item::Function(FunctionItem(function)),
            None => return gap("a function that is not a global def of a loaded .bzl file"),
        }
    } else if let Some(structure) = StructRef::from_value(value) {
        let mut fields = structure
            .iter()
            .map(|(name, value)| {
                Ok((
                    name.as_str().to_owned(),
                    snapshot(value, functions, depth + 1)?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Item::Struct(Arc::new(StructItem {
            provider: None,
            fields,
            location: None,
        }))
    } else if let Some((identity, name, fields, location)) = crate::providers::instance_parts(value)
    {
        Item::Struct(Arc::new(StructItem {
            provider: Some((identity, name)),
            fields: fields
                .into_iter()
                .map(|(name, value)| Ok((name, snapshot(value, functions, depth + 1)?)))
                .collect::<anyhow::Result<Vec<_>>>()?,
            location,
        }))
    } else if let Some(list) = ListRef::from_value(value) {
        // Only a frozen list passes the element check; a frozen list's
        // contents are frozen too.
        if value.unpack_frozen().is_none() {
            bail!("depset elements must not be mutable values");
        }
        Item::List(
            list.iter()
                .map(|item| snapshot(item, functions, depth + 1))
                .collect::<anyhow::Result<_>>()?,
        )
    } else if let Some(dict) = DictRef::from_value(value) {
        if value.unpack_frozen().is_none() {
            bail!("depset elements must not be mutable values");
        }
        Item::Dict(DictItem(
            dict.iter()
                .map(|(key, value)| {
                    Ok((
                        snapshot(key, functions, depth + 1)?,
                        snapshot(value, functions, depth + 1)?,
                    ))
                })
                .collect::<anyhow::Result<_>>()?,
        ))
    } else if value.get_type() == "set" {
        return gap("a frozen set (immutable in Bazel; no owned native snapshot)");
    } else {
        return gap(&format!("a value of type '{}'", value.get_type()));
    })
}

/// Evaluates `code` with the analysis globals and Starlark's `struct`
/// (crate::starlark_types tests).
#[cfg(test)]
pub(crate) fn tests_eval(code: &str) -> Result<String, String> {
    tests::eval(code)
}

#[cfg(test)]
mod tests {
    use starlark::environment::{Globals, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    pub(super) fn eval(code: &str) -> Result<String, String> {
        let globals = crate::starlark_types::analysis_test_globals();
        let ast = AstModule::parse("t.bzl", code.to_owned(), &Dialect::Standard)
            .map_err(|e| e.to_string())?;
        Module::with_temp_heap(|module| {
            let mut evaluator = Evaluator::new(&module);
            evaluator
                .eval_module(ast, &globals)
                .map(|value| value.to_str())
                .map_err(|e| e.to_string())
        })
    }

    #[test]
    fn structs_are_snapshotted_with_struct_semantics() {
        assert_eq!(
            eval(
                "s = struct(b = (1, None), a = 'x')\nd = depset([s, struct(a = 'x', b = (1, None)), struct(a = 'y', b = ())])\nl = d.to_list()\n\"%s %s %s %s %s\" % (len(l), l[0] == s, l[0], type(l[1]), repr(d))"
            ),
            Ok("2 True struct(a = \"x\", b = (1, None)) struct depset([struct(a = \"x\", b = (1, None)), struct(a = \"y\", b = ())])".to_owned())
        );
        // A mutable field value, a top-level list, a built-in function.
        for (code, message) in [
            (
                "depset([struct(x = [1])])",
                "depset elements must not be mutable values",
            ),
            (
                "depset([[1]])",
                "depset elements must not be mutable values",
            ),
            (
                "depset([(1, [2])])",
                "depset elements must not be mutable values",
            ),
            (
                "depset([struct(f = len)])",
                "depset elements must not be mutable values",
            ),
            (
                "depset([struct(a = 1), 'x'])",
                "cannot add an item of type 'string' to a depset of 'struct'",
            ),
            (
                "depset(['x'], transitive = [depset([struct()])])",
                "cannot add an item of type 'struct' to a depset of 'string'",
            ),
        ] {
            let error = eval(code).unwrap_err();
            assert!(error.contains(message), "{code}: {error}");
        }
        // Depsets compare by identity (paired capture O/reports).
        assert_eq!(
            eval(
                "\"%s %s %s\" % (len(depset([struct(d = depset([1])), struct(d = depset([1]))]).to_list()), depset([1]) == depset([1]), depset() == depset())"
            ),
            Ok("2 False True".to_owned())
        );
        // Nested depsets and None are values too.
        assert_eq!(
            eval(
                "i = depset([1])\nd = depset([struct(d = i, n = None), struct(d = i, n = None)])\nl = d.to_list()\n\"%s %s %s\" % (len(l), l[0].d == i, l[0].n)"
            ),
            Ok("1 True None".to_owned())
        );
    }

    /// Evaluates `code` with the globals of a frozen module (its lists and
    /// dicts are frozen, like a loaded `.bzl` file's or a dependency's).
    fn eval_with_frozen(frozen_code: &str, code: &str) -> Result<String, String> {
        let globals = crate::starlark_types::analysis_test_globals();
        let frozen = Module::with_temp_heap(|module| {
            let ast = AstModule::parse("f.bzl", frozen_code.to_owned(), &Dialect::Standard)
                .map_err(|e| e.to_string())?;
            Evaluator::new(&module)
                .eval_module(ast, &globals)
                .map_err(|e| e.to_string())?;
            module.freeze().map_err(|e| format!("{e:?}"))
        })?;
        let ast = AstModule::parse("t.bzl", code.to_owned(), &Dialect::Standard)
            .map_err(|e| e.to_string())?;
        Module::with_temp_heap(|module| {
            for name in frozen.names() {
                let value = frozen.get(name.as_str()).unwrap();
                module.set(
                    name.as_str(),
                    module.heap().access_owned_frozen_value(&value),
                );
            }
            let mut evaluator = Evaluator::new(&module);
            evaluator
                .eval_module(ast, &globals)
                .map(|value| value.to_str())
                .map_err(|e| e.to_string())
        })
    }

    #[test]
    fn frozen_lists_and_dicts_inside_elements() {
        const FROZEN: &str = "L = [1, ('t',)]\nL2 = [1, ('t',)]\nD = {'b': 2, 'a': [1]}\nD2 = {'a': [1], 'b': 2}\nN = [[1], {'k': [2]}]";
        // Equality element-wise / by contents; a list never equals a tuple;
        // dict insertion order kept, not compared.
        assert_eq!(
            eval_with_frozen(
                FROZEN,
                "l = depset([struct(l = L), struct(l = L2), struct(l = tuple(L)), struct(m = D), struct(m = D2)]).to_list()\n\"%s %r %s %s %s\" % (len(l), l, type(l[0].l), l[0].l == L, l[2].m == D2)"
            ),
            Ok("3 [struct(l = [1, (\"t\",)]), struct(l = (1, (\"t\",))), struct(m = {\"b\": 2, \"a\": [1]})] list True True".to_owned())
        );
        // The re-allocated containers are frozen, also nested ones.
        for (code, message) in [
            (
                "depset([struct(l = L)]).to_list()[0].l.append(1)",
                "trying to mutate a frozen list value",
            ),
            (
                "depset([struct(n = N)]).to_list()[0].n[0].append(1)",
                "trying to mutate a frozen list value",
            ),
            (
                "depset([struct(n = N)]).to_list()[0].n[1].pop('k')",
                "trying to mutate a frozen dict value",
            ),
            (
                "depset([struct(m = D)]).to_list()[0].m['a'].append(2)",
                "trying to mutate a frozen list value",
            ),
            // Top-level frozen lists and dicts stay rejected.
            ("depset([L])", "depsets cannot contain items of type 'list'"),
            ("depset([D])", "depsets cannot contain items of type 'dict'"),
            // A mutable list is still a mutable element.
            (
                "depset([struct(l = [1])])",
                "depset elements must not be mutable values",
            ),
            // Lambdas stay explicit.
            (
                "depset([struct(f = lambda: 1)])",
                "is not implemented in the native profile",
            ),
        ] {
            let error = eval_with_frozen(FROZEN, code).unwrap_err();
            assert!(error.contains(message), "{code}: {error}");
        }
        // A tuple holding a frozen list may be a top-level element.
        assert_eq!(
            eval_with_frozen(FROZEN, "repr(depset([(L,), (L2,), (N,)]))"),
            Ok("depset([([1, (\"t\",)],), ([[1], {\"k\": [2]}],)])".to_owned())
        );
    }

    #[test]
    fn existing_element_kinds_keep_their_order_and_dedup() {
        assert_eq!(
            eval("str(depset(['b', 'a', 'b'], transitive = [depset(['c', 'a'])]).to_list())"),
            Ok("[\"c\", \"a\", \"b\"]".to_owned())
        );
        let _ = Globals::standard();
    }
}
