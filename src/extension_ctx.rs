//! Bounded ordinary extension callbacks with an immutable, separately frozen
//! MODULE projection. Repository calls only record declarations: publication,
//! sibling mapping, schema binding and materialization belong to the caller.

// The generated native wrapper preserves Bazel's ten-parameter download API.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{anyhow, bail, ensure};
use serde::Serialize;
use starlark::collections::SmallMap;
use starlark::collections::StarlarkHasher;
use starlark::environment::{Methods, MethodsBuilder, Module};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::UnpackTuple;
use starlark::values::{FrozenHeapName, Heap, OwnedFrozenValue, Value, ValueLike};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;

use crate::custom_rule::AttrValue;
use crate::extension_calls::{CallRecorder, RecordedRepository};
use crate::label::{Label, PackageId};
use crate::module_extension::ExtensionDefinition;
use crate::repository::RepositorySet;
use crate::repository_budget::HostBudget;
use crate::repository_catalog::OrdinaryParticipant;
use crate::starlark_types::alloc_label;

#[derive(Debug)]
pub struct ExtensionExecution {
    pub repositories: Vec<RecordedRepository>,
    pub modules: Vec<BoundModule>,
    /// Recorded StarlarkBaseExternalContext inputs: watched files, reads,
    /// host facilities, and the digest of declared/getenv environment
    /// observations (values never kept).
    pub(crate) inputs: crate::repository_ctx::ExecutionInputs,
    /// None when the callback returned None (ModuleExtensionMetadata.DEFAULT).
    pub metadata: Option<crate::extension_metadata::ExtensionMetadata>,
    /// Repository mapping lookups of label-typed tag attributes
    /// (StarlarkBazelModule.create's recorder, RegularRunnableExtension.java:352-365).
    pub tag_repo_mappings: Vec<crate::loading::RecordedRepoMapping>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BoundModule {
    pub owner: String,
    pub name: String,
    pub version: String,
    pub is_root: bool,
    pub tags: BTreeMap<String, Vec<BoundTag>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BoundTag {
    pub attributes: BTreeMap<String, AttrValue>,
    pub dev_dependency: bool,
    pub ordinal: usize,
}

// Only derives generate unsafe marker/coercion implementations. Every Starlark
// reference is traced and frozen; the host context contains only owned data.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::starlark_complex_value;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, ValueError, starlark_value,
    };

    #[derive(ProvidesStaticType)]
    pub(super) struct Context {
        pub repositories: Arc<RepositorySet>,
        pub public_label: Label,
        pub calls: CallRecorder,
        /// The StarlarkBaseExternalContext half of module_ctx (working
        /// directory, downloads, execute, watches, declared `environ` and
        /// `getenv` observations), shared with repository_ctx.
        pub base: crate::repository_ctx::Context,
    }

    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct TagGen<V> {
        pub attrs: V,
        pub dev_dependency: bool,
        /// TypeCheckedTag.SortKey (TypeCheckedTag.java:46-69): the module's
        /// BFS index and the tag's index in its module's usage.
        pub module_index: u32,
        pub tag_index: u32,
    }

    impl<V> fmt::Display for TagGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // TypeCheckedTag has no repr override (bazel/bzlmod/TypeCheckedTag.java:39;
            // StarlarkValue.java:50-52).
            f.write_str(
                "<unknown object com.google.devtools.build.lib.bazel.bzlmod.TypeCheckedTag>",
            )
        }
    }

    starlark_complex_value!(pub(super) Tag);

    // TypeCheckedTag is `@StarlarkBuiltin(name = "bazel_module_tag")`
    // (bazel/bzlmod/TypeCheckedTag.java:38).
    #[starlark_value(type = "bazel_module_tag")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TagGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            // attrs is always an immutable struct containing only schema fields.
            // The hidden dev flag never shadows a declared dev_dependency attr.
            self.attrs.to_value().get_attr(name, heap).ok().flatten()
        }

        /// TypeCheckedTag is a Structure (Structure.java:78-81).
        fn set_attr(&self, _attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
            crate::runfiles::no_field_assignment("bazel_module_tag")
        }

        fn dir_attr(&self) -> Vec<String> {
            self.attrs.to_value().dir_attr()
        }
    }

    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct BazelModuleGen<V> {
        pub name: String,
        pub version: String,
        pub is_root: bool,
        pub tags: V,
    }

    impl<V> fmt::Display for BazelModuleGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // StarlarkBazelModule has no repr override
            // (bazel/bzlmod/StarlarkBazelModule.java:45; StarlarkValue.java:50-52).
            f.write_str(
                "<unknown object com.google.devtools.build.lib.bazel.bzlmod.StarlarkBazelModule>",
            )
        }
    }

    starlark_complex_value!(pub(super) BazelModule);

    /// `bazel_module.tags` (StarlarkBazelModule.Tags, bazel/bzlmod/StarlarkBazelModule.java:51-94):
    /// the tag lists by tag class name.
    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct TagsGen<V> {
        pub classes: V,
    }

    impl<V> fmt::Display for TagsGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // No repr override (StarlarkValue.java:50-52).
            f.write_str(
                "<unknown object com.google.devtools.build.lib.bazel.bzlmod.StarlarkBazelModule$Tags>",
            )
        }
    }

    starlark_complex_value!(pub(super) Tags);

    #[starlark_value(type = "bazel_module_tags")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TagsGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            self.classes.to_value().get_attr(name, heap).ok().flatten()
        }

        fn dir_attr(&self) -> Vec<String> {
            self.classes.to_value().dir_attr()
        }

        /// StarlarkBazelModule.Tags is a Structure (StarlarkBazelModule.java:66,
        /// Structure.java:78-81).
        fn set_attr(&self, _attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
            crate::runfiles::no_field_assignment("bazel_module_tags")
        }
    }

    #[starlark_value(type = "bazel_module")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for BazelModuleGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            Some(match name {
                "name" => heap.alloc(self.name.as_str()),
                "version" => heap.alloc(self.version.as_str()),
                "is_root" => heap.alloc(self.is_root),
                "tags" => self.tags.to_value(),
                _ => return None,
            })
        }

        fn dir_attr(&self) -> Vec<String> {
            ["name", "version", "is_root", "tags"]
                .map(str::to_owned)
                .to_vec()
        }
    }

    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct ModuleCtxGen<V> {
        pub modules: V,
        pub os: V,
        pub facts: V,
        pub root_has_non_dev_dependency: bool,
    }

    impl<V> fmt::Display for ModuleCtxGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // ModuleExtensionContext has no repr override
            // (bazel/bzlmod/ModuleExtensionContext.java:51; StarlarkValue.java:50-52).
            f.write_str("<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleExtensionContext>")
        }
    }

    starlark_complex_value!(pub(super) ModuleCtx);

    #[starlark_value(type = "module_ctx")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ModuleCtxGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_methods() -> Option<&'static Methods> {
            Some(MODULE_METHODS.methods())
        }

        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            match name {
                "modules" => Some(self.modules.to_value()),
                "os" => Some(self.os.to_value()),
                "root_module_has_non_dev_dependency" => {
                    Some(heap.alloc(self.root_has_non_dev_dependency))
                }
                "facts" => Some(self.facts.to_value()),
                // Unsupported context facilities are absent, never dummy values.
                _ => None,
            }
        }

        fn dir_attr(&self) -> Vec<String> {
            // Methods (StarlarkBaseExternalContext's and extension_metadata,
            // is_dev_dependency, tag_sort_key) come from MODULE_METHODS.
            [
                "modules",
                "os",
                "root_module_has_non_dev_dependency",
                "facts",
            ]
            .map(str::to_owned)
            .to_vec()
        }
    }

    /// TypeCheckedTag.SortKey (TypeCheckedTag.java:41-69): opaque, immutable,
    /// ordered by (module index, tag index); repr `<sort_key>`, print()
    /// `<sort_key module=M tag=T>`. A Java record: equal by both indices.
    #[derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        StarlarkPagablePanic,
    )]
    pub(super) struct SortKey {
        pub module_index: u32,
        pub tag_index: u32,
    }
    starlark::starlark_simple_value!(SortKey);

    impl fmt::Display for SortKey {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<sort_key>")
        }
    }

    #[starlark_value(type = "sort_key")]
    impl<'v> StarlarkValue<'v> for SortKey {
        fn collect_debug_print(&self, collector: &mut String, _eval: &Evaluator<'v, '_, '_>) {
            collector.push_str(&format!(
                "<sort_key module={} tag={}>",
                self.module_index, self.tag_index
            ));
        }

        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(other.downcast_ref::<Self>() == Some(self))
        }

        fn compare(&self, other: Value<'v>) -> starlark::Result<std::cmp::Ordering> {
            match other.downcast_ref::<Self>() {
                Some(other) => Ok(self.cmp(other)),
                None => ValueError::unsupported_with(self, "compare", other),
            }
        }

        fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
            self.hash(hasher);
            Ok(())
        }
    }

    /// Bazel `Facts` (bazel/bzlmod/Facts.java): the facts persisted in the
    /// lockfile for this extension (SingleExtensionEvalFunction.java:145-183),
    /// Facts.EMPTY without any.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Facts {
        #[allocative(skip)]
        pub members: Vec<(String, crate::lockfile::Json)>,
    }
    starlark::starlark_simple_value!(Facts);

    impl fmt::Display for Facts {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // Facts.java repr: contents are not leaked to Starlark.
            f.write_str("Facts(<opaque, inspect with print()>)")
        }
    }

    impl Facts {
        pub(super) fn get(&self, key: &str) -> Option<&crate::lockfile::Json> {
            self.members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value)
        }
    }

    #[starlark_value(type = "Facts")]
    impl<'v> StarlarkValue<'v> for Facts {
        /// Facts.debugPrint (bazel/bzlmod/Facts.java:162-168): `Facts(<repr>)`.
        fn collect_debug_print(&self, collector: &mut String, eval: &Evaluator<'v, '_, '_>) {
            let value = super::alloc_json(
                eval.heap(),
                &crate::lockfile::Json::Object(self.members.clone()),
            )
            .map(|value| value.to_repr())
            .unwrap_or_else(|error| format!("<{error}>"));
            collector.push_str(&format!("Facts({value})"));
        }

        fn get_methods() -> Option<&'static Methods> {
            Some(FACTS_METHODS.methods())
        }

        fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
            match index.unpack_str().and_then(|key| self.get(key)) {
                Some(value) => super::alloc_json(heap, value).map_err(starlark::Error::new_other),
                // Bazel's getIndex yields Java null for a missing key; fail explicitly.
                None => Err(starlark::Error::new_other(anyhow!(
                    "Facts has no key {}",
                    index.to_repr()
                ))),
            }
        }

        fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(other
                .unpack_str()
                .is_some_and(|key| self.get(key).is_some()))
        }

        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            // AutoValue Facts.equals: Dict equality of the values.
            Ok(other.downcast_ref::<Facts>().is_some_and(|other| {
                crate::lockfile::Json::Object(other.members.clone())
                    == crate::lockfile::Json::Object(self.members.clone())
            }))
        }

        fn write_hash(&self, _hasher: &mut StarlarkHasher) -> starlark::Result<()> {
            Err(starlark::Error::new_other(anyhow!(
                "unhashable type: 'Facts'"
            )))
        }
    }

    /// Opaque Bazel `extension_metadata` (ModuleExtensionMetadata.java). Its
    /// validated direct-dependency payload drives the root use_repo fixup
    /// after the callback; facts are validated and discarded (no lockfile).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct ExtensionMetadata {
        pub metadata: crate::extension_metadata::ExtensionMetadata,
    }
    starlark::starlark_simple_value!(ExtensionMetadata);

    impl fmt::Display for ExtensionMetadata {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // No repr override in Bazel: StarlarkValue's default for AutoValue.
            f.write_str(
                "<unknown object com.google.devtools.build.lib.bazel.bzlmod.AutoValue_ModuleExtensionMetadata>",
            )
        }
    }

    #[starlark_value(type = "extension_metadata")]
    impl<'v> StarlarkValue<'v> for ExtensionMetadata {
        fn write_hash(&self, _hasher: &mut StarlarkHasher) -> starlark::Result<()> {
            Err(starlark::Error::new_other(anyhow!(
                "unhashable type: 'extension_metadata'"
            )))
        }
    }
}
use values::{
    BazelModuleGen, Context, ExtensionMetadata, FrozenTag, ModuleCtxGen, SortKey, Tag, TagGen,
    TagsGen,
};

fn context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> Option<&'a Context> {
    eval.extra.and_then(|extra| extra.downcast_ref::<Context>())
}

/// Starlark value of a facts JSON value (Json.decode of FactsAdapter.read):
/// strings are Bazel's Latin-1 internal strings of their UTF-8 bytes.
pub(crate) fn alloc_json<'v>(
    heap: Heap<'v>,
    value: &crate::lockfile::Json,
) -> anyhow::Result<Value<'v>> {
    use crate::lockfile::Json;
    let text = |text: &str| crate::host_os::latin1_text(text.as_bytes());
    Ok(match value {
        Json::Null => Value::new_none(),
        Json::Bool(b) => Value::new_bool(*b),
        Json::Number(number) => {
            if let Ok(int) = number.parse::<i32>() {
                heap.alloc(int)
            } else if let Ok(int) = number.parse::<i64>() {
                heap.alloc(int)
            } else if number.contains(['.', 'e', 'E']) {
                heap.alloc(
                    number
                        .parse::<f64>()
                        .map_err(|_| anyhow!("invalid facts number {number}"))?,
                )
            } else {
                bail!("facts integer {number} exceeds 64 bits; unsupported by the native profile")
            }
        }
        Json::String(value) => heap.alloc(text(value)),
        Json::Array(items) => {
            let items = items
                .iter()
                .map(|item| alloc_json(heap, item))
                .collect::<anyhow::Result<Vec<_>>>()?;
            heap.alloc(starlark::values::list::AllocList(items))
        }
        Json::Object(members) => {
            let mut dict = SmallMap::new();
            for (key, item) in members {
                let key = heap.alloc_str(&text(key)).to_value();
                dict.insert_hashed(
                    key.get_hashed().map_err(|error| anyhow!("{error}"))?,
                    alloc_json(heap, item)?,
                );
            }
            heap.alloc(starlark::values::dict::Dict::new(dict))
        }
    })
}

/// Record a `Label()` repository mapping lookup made while a module
/// extension's implementation runs; false outside one.
pub(crate) fn record_repo_mapping(
    eval: &Evaluator<'_, '_, '_>,
    entry: &crate::loading::RecordedRepoMapping,
) -> bool {
    let Some(context) = context(eval) else {
        return false;
    };
    context
        .base
        .record_bazel(crate::lockfile_extensions::RecordedInput::repo_mapping(
            &entry.from,
            &entry.apparent,
            entry.canonical.clone(),
        ));
    true
}

/// The StarlarkBaseExternalContext of a running module extension.
pub(crate) fn base_context<'a>(
    eval: &'a Evaluator<'_, '_, '_>,
) -> Option<&'a crate::repository_ctx::Context> {
    context(eval).map(|context| &context.base)
}

/// Whether `value` is a module_ctx.
pub(crate) fn is_module_ctx(value: Value<'_>) -> bool {
    values::ModuleCtx::from_value(value).is_some()
}

/// True while evaluating a module-extension callback (byte-string profile).
pub(crate) fn evaluating_callback(eval: &Evaluator<'_, '_, '_>) -> bool {
    context(eval).is_some()
}

/// Bazel's extension-evaluation no-op marker is dynamic, not a property of the
/// function's MODULE-loaded globals. Initialization and repository evaluators
/// carry different context types and must not receive empty introspection data.
pub(crate) fn require_extension_introspection(
    eval: &Evaluator<'_, '_, '_>,
    function_name: &str,
) -> anyhow::Result<()> {
    ensure!(
        context(eval).is_some(),
        "{function_name}() can only be used while evaluating a BUILD file, a legacy macro, or a rule finalizer"
    );
    Ok(())
}

pub(crate) fn repository_context(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    context(eval).map(|context| context.repositories.clone())
}

pub(crate) fn definition_package(eval: &Evaluator<'_, '_, '_>) -> Option<PackageId> {
    let context = context(eval)?;
    Some(
        eval.call_stack_top_location()
            .and_then(|location| Label::parse_canonical(location.filename()).ok())
            .unwrap_or_else(|| context.public_label.clone())
            .package_id(),
    )
}

pub(crate) fn record_repository_call<'v>(
    me: Value<'v>,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    let context = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<Context>())
        .ok_or_else(|| {
            anyhow!(
                "repository rule calls are unsupported outside recorded MODULE/direct execution"
            )
        })?;
    context.calls.record(me, args, &*eval)
}

starlark::methods_static!(MODULE_METHODS = all_module_methods);

fn all_module_methods(builder: &mut MethodsBuilder) {
    crate::repository_ctx::base_methods(builder);
    module_methods(builder);
}

/// ModuleExtensionContext-only members (ModuleExtensionContext.java:117-321).
#[starlark_module]
fn module_methods(builder: &mut MethodsBuilder) {
    fn extension_metadata<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<ExtensionMetadata> {
        let _ = this;
        Ok(ExtensionMetadata {
            metadata: extension_metadata_call(&args.items, &kwargs, heap)?,
        })
    }

    fn is_dev_dependency<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] tag: Value<'v>,
    ) -> anyhow::Result<bool> {
        let _ = this;
        // Nominal type checking only: any genuine tag is accepted, including a
        // tag from another projection. No structural or group-membership test.
        if let Some(tag) = tag.downcast_ref::<Tag>() {
            Ok(tag.dev_dependency)
        } else if let Some(tag) = tag.downcast_ref::<FrozenTag>() {
            Ok(tag.dev_dependency)
        } else {
            bail!("is_dev_dependency expects a tag, got {}", tag.get_type())
        }
    }

    /// ModuleExtensionContext.getTagSortKey (java:163-184).
    fn tag_sort_key<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] tag: Value<'v>,
    ) -> anyhow::Result<SortKey> {
        let _ = this;
        let (module_index, tag_index) = if let Some(tag) = tag.downcast_ref::<Tag>() {
            (tag.module_index, tag.tag_index)
        } else if let Some(tag) = tag.downcast_ref::<FrozenTag>() {
            (tag.module_index, tag.tag_index)
        } else {
            bail!(
                "in call to tag_sort_key(), parameter 'tag' got value of type '{}', want 'bazel_module_tag'",
                tag.get_type()
            )
        };
        Ok(SortKey {
            module_index,
            tag_index,
        })
    }
}

starlark::methods_static!(FACTS_METHODS = facts_methods);

#[starlark_module]
fn facts_methods(builder: &mut MethodsBuilder) {
    /// Facts.java `get(key, default=None)`.
    fn get<'v>(
        this: Value<'v>,
        key: &str,
        default: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let facts = this
            .downcast_ref::<values::Facts>()
            .ok_or_else(|| anyhow!("get requires a Facts receiver"))?;
        match facts.get(key) {
            Some(value) => alloc_json(heap, value),
            None => Ok(default.unwrap_or_else(Value::new_none)),
        }
    }
}

/// ModuleExtensionContext.java:207-321 parameter order. All are named-only.
const METADATA_PARAMS: [&str; 4] = [
    "root_module_direct_deps",
    "root_module_direct_dev_deps",
    "reproducible",
    "facts",
];

/// BuiltinFunction.ArgumentProcessor for extension_metadata, then
/// ModuleExtensionContext.extensionMetadata: named arguments are matched and
/// type-checked in CALL order, unexpected positionals are reported afterwards
/// (BuiltinFunction.java:310-430), then Dict.cast(facts), then create().
fn extension_metadata_call<'v>(
    positional: &[Value<'v>],
    named: &SmallMap<String, Value<'v>>,
    heap: Heap<'v>,
) -> anyhow::Result<crate::extension_metadata::ExtensionMetadata> {
    let mut values: [Option<Value<'_>>; 4] = [None; 4];
    for (name, value) in named.iter() {
        let Some(index) = METADATA_PARAMS.iter().position(|param| param == name) else {
            bail!(
                "extension_metadata() got unexpected keyword argument '{name}'{}",
                did_you_mean(name, &METADATA_PARAMS)
            );
        };
        let (ok, want) = match index {
            0 | 1 => (
                value.is_none() || value.unpack_str().is_some() || is_sequence(*value),
                "sequence, string, or NoneType",
            ),
            2 => (value.unpack_bool().is_some(), "bool"),
            _ => (
                starlark::values::dict::DictRef::from_value(*value).is_some(),
                "dict",
            ),
        };
        ensure!(
            ok,
            "in call to extension_metadata(), parameter '{}' got value of type '{}', want '{want}'",
            METADATA_PARAMS[index],
            value.get_type()
        );
        // Duplicate keywords are rejected by the evaluator before the call.
        values[index] = Some(*value);
    }
    ensure!(
        positional.is_empty(),
        "extension_metadata() got unexpected positional argument"
    );
    let none = Value::new_none();
    let reproducible = values[2].and_then(|value| value.unpack_bool()) == Some(true);
    // ModuleExtensionMetadata.create: facts are validated before the
    // direct-dependency arguments (Dict.cast precedes create()).
    let mut has_facts = false;
    let mut facts_json = None;
    if let Some(facts) = values[3] {
        validate_facts(facts)?;
        has_facts =
            starlark::values::dict::DictRef::from_value(facts).is_some_and(|dict| !dict.is_empty());
        if has_facts {
            // json.encode sorts dict keys and writes tuples as lists, so it
            // equals the encoding of Facts.validateAndNormalize's value.
            let encoded = crate::json::encode_value(facts, heap)?;
            facts_json = Some(
                crate::lockfile_extensions::internal_to_unicode(&encoded)
                    .map_err(anyhow::Error::msg)?,
            );
        }
    }
    let mut metadata = create_metadata(
        values[0].unwrap_or(none),
        values[1].unwrap_or(none),
        reproducible,
    )?;
    metadata.has_facts = has_facts;
    metadata.facts = facts_json;
    Ok(metadata)
}

/// Java Sequence implementations reachable from Starlark: list, tuple, range.
fn is_sequence(value: Value<'_>) -> bool {
    use starlark::values::list::ListRef;
    use starlark::values::tuple::TupleRef;
    ListRef::from_value(value).is_some()
        || TupleRef::from_value(value).is_some()
        || value.get_type() == "range"
}

/// StarlarkList.equals(StarlarkList.immutableOf()): lists only, never tuples.
fn is_empty_list(value: Value<'_>) -> bool {
    starlark::values::list::ListRef::from_value(value).is_some_and(|list| list.is_empty())
}

/// Sequence.cast(x, String.class, what) (Sequence.java:132-150).
fn cast_strings(value: Value<'_>, what: &str) -> anyhow::Result<Vec<String>> {
    use starlark::values::list::ListRef;
    use starlark::values::tuple::TupleRef;
    let items: Vec<Value<'_>> = if let Some(list) = ListRef::from_value(value) {
        list.content().to_vec()
    } else if let Some(tuple) = TupleRef::from_value(value) {
        tuple.content().to_vec()
    } else if value.get_type() == "range" {
        // Every element of a range is an int: only an empty range casts.
        if value.length().map_err(|error| anyhow!(error.to_string()))? > 0 {
            bail!("at index 0 of {what}, got element of type int, want string");
        }
        Vec::new()
    } else {
        bail!("for {what}, got {}, want sequence", value.get_type());
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            item.unpack_str().map(str::to_owned).ok_or_else(|| {
                anyhow!(
                    "at index {index} of {what}, got element of type {}, want string",
                    item.get_type()
                )
            })
        })
        .collect()
}

/// ModuleExtensionMetadata.create(Object, Object, boolean, Dict) after facts
/// (ModuleExtensionMetadata.java:96-174).
fn create_metadata(
    deps: Value<'_>,
    dev_deps: Value<'_>,
    reproducible: bool,
) -> anyhow::Result<crate::extension_metadata::ExtensionMetadata> {
    use crate::extension_metadata::{
        ExtensionMetadata, UseAllRepos, validate_user_provided_repo_name,
    };
    let mode = |use_all_repos| ExtensionMetadata {
        explicit_root_module_direct_deps: None,
        explicit_root_module_direct_dev_deps: None,
        use_all_repos,
        reproducible,
        has_facts: false,
        facts: None,
    };
    if deps.is_none() && dev_deps.is_none() {
        return Ok(mode(UseAllRepos::No));
    }
    let all = |value: Value<'_>| value.unpack_str() == Some("all");
    if all(deps) && is_empty_list(dev_deps) {
        return Ok(mode(UseAllRepos::Regular));
    }
    if all(dev_deps) && is_empty_list(deps) {
        return Ok(mode(UseAllRepos::Dev));
    }
    ensure!(
        !all(deps) && !all(dev_deps),
        "if one of root_module_direct_deps and root_module_direct_dev_deps is \"all\", the other must be an empty list"
    );
    ensure!(
        deps.unpack_str().is_none() && dev_deps.unpack_str().is_none(),
        "root_module_direct_deps and root_module_direct_dev_deps must be None, \"all\", or a list of strings"
    );
    ensure!(
        deps.is_none() == dev_deps.is_none(),
        "root_module_direct_deps and root_module_direct_dev_deps must both be specified or both be unspecified"
    );
    let deps = cast_strings(deps, "root_module_direct_deps")?;
    let dev_deps = cast_strings(dev_deps, "root_module_direct_dev_deps")?;
    let mut explicit_deps: Vec<String> = Vec::new();
    for dep in deps {
        validate_user_provided_repo_name(&dep)
            .map_err(|error| anyhow!("in root_module_direct_deps: {error}"))?;
        ensure!(
            !explicit_deps.contains(&dep),
            "in root_module_direct_deps: duplicate entry '{dep}'"
        );
        explicit_deps.push(dep);
    }
    let mut explicit_dev_deps: Vec<String> = Vec::new();
    for dep in dev_deps {
        validate_user_provided_repo_name(&dep)
            .map_err(|error| anyhow!("in root_module_direct_dev_deps: {error}"))?;
        ensure!(
            !explicit_deps.contains(&dep),
            "in root_module_direct_dev_deps: entry '{dep}' is also in root_module_direct_deps"
        );
        ensure!(
            !explicit_dev_deps.contains(&dep),
            "in root_module_direct_dev_deps: duplicate entry '{dep}'"
        );
        explicit_dev_deps.push(dep);
    }
    Ok(ExtensionMetadata {
        explicit_root_module_direct_deps: Some(explicit_deps),
        explicit_root_module_direct_dev_deps: Some(explicit_dev_deps),
        use_all_repos: UseAllRepos::No,
        reproducible,
        has_facts: false,
        facts: None,
    })
}

/// SpellChecker.didYouMean (net/starlark/java/spelling/SpellChecker.java).
fn did_you_mean(input: &str, words: &[&str]) -> String {
    crate::native_rules::did_you_mean(input, words)
}

/// Facts.java MAX_FACTS_DEPTH.
const MAX_FACTS_DEPTH: i32 = 7;

/// Facts.validateAndCreate (Facts.java), preceded by its Dict.cast of the
/// `facts` parameter. The validated value is discarded (lockfile mode off).
fn validate_facts(value: Value<'_>) -> anyhow::Result<()> {
    use starlark::values::dict::DictRef;
    let dict = DictRef::from_value(value).ok_or_else(|| {
        anyhow!(
            "in call to extension_metadata(), parameter 'facts' got value of type '{}', want 'dict'",
            value.get_type()
        )
    })?;
    for (key, entry) in dict.iter() {
        if key.unpack_str().is_none() {
            bail!(
                "got dict<{}, {}> for 'facts', want dict<string, unknown>",
                key.get_type(),
                entry.get_type()
            );
        }
    }
    drop(dict);
    validate_fact(value, MAX_FACTS_DEPTH)
}

fn validate_fact(value: Value<'_>, remaining: i32) -> anyhow::Result<()> {
    use starlark::values::dict::DictRef;
    use starlark::values::list::ListRef;
    use starlark::values::tuple::TupleRef;
    if remaining < 0 {
        bail!("Facts cannot be nested more than {MAX_FACTS_DEPTH} levels deep");
    }
    if value.is_none()
        || value.unpack_str().is_some()
        || value.unpack_bool().is_some()
        || matches!(value.get_type(), "int" | "float")
    {
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        return list
            .iter()
            .try_for_each(|item| validate_fact(item, remaining - 1));
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return tuple
            .iter()
            .try_for_each(|item| validate_fact(item, remaining - 1));
    }
    if let Some(dict) = DictRef::from_value(value) {
        for (key, entry) in dict.iter() {
            if key.unpack_str().is_none() {
                bail!(
                    "Facts keys must be strings, got '{}: {}' ({})",
                    key.to_repr(),
                    entry.to_repr(),
                    key.get_type()
                );
            }
            validate_fact(entry, remaining - 1)?;
        }
        return Ok(());
    }
    bail!(
        "'{}' ({}) is not supported in facts",
        value.to_repr(),
        value.get_type()
    )
}

fn bind_modules(
    repositories: &RepositorySet,
    definition: &ExtensionDefinition,
    participants: &[OrdinaryParticipant],
    recorded: &RefCell<Vec<crate::loading::RecordedRepoMapping>>,
) -> Result<Vec<BoundModule>, String> {
    // Check every source profile before binding/allocating any projection.
    for participant in participants {
        if let Some(error) = &participant.source_profile_error {
            return Err(format!("MODULE owner {:?}: {error}", participant.owner));
        }
        if !participant.owner.is_ascii()
            || !participant.name.is_ascii()
            || !participant.version.is_ascii()
        {
            return Err(
                "non-ASCII module metadata is unsupported in module extension execution".to_owned(),
            );
        }
    }
    // Preflight the *whole group* while everything is borrowed. In particular,
    // empty raw tags still expand defaults; charging only raw JSON misses that
    // amplification. This also reserves projection/freeze and provenance space.
    let mut budget = HostBudget::unbounded();
    for participant in participants {
        budget.node()?;
        budget.string(&participant.owner)?;
        budget.string(&participant.name)?;
        budget.string(&participant.version)?;
        for name in definition.tag_classes.keys() {
            budget.string(name)?;
        }
        for tag in &participant.tags {
            budget.node()?;
            budget.string(&tag.name)?;
            if let Some(schema) = definition.tag_classes.get(&tag.name) {
                budget.schema(schema)?;
            }
            // Unknown None is still omitted by the binder, but its raw capture
            // occupies memory and must count toward the preparation profile.
            for (name, value) in &tag.attributes {
                budget.string(name)?;
                budget.json(value, 0)?;
            }
        }
    }
    let resolution_budget = RefCell::new(HostBudget::unbounded());
    participants
        .iter()
        .map(|participant| {
            let mut tags: BTreeMap<String, Vec<BoundTag>> = definition
                .tag_classes
                .keys()
                .map(|name| (name.clone(), Vec::new()))
                .collect();
            let mut ordered: Vec<_> = participant.tags.iter().collect();
            ordered.sort_by_key(|tag| tag.ordinal);
            for tag in ordered {
                let schema = definition.tag_classes.get(&tag.name).ok_or_else(|| {
                    format!(
                        "unknown tag class {:?} in MODULE owner {:?}",
                        tag.name, participant.owner
                    )
                })?;
                let repo_mapping_where =
                    format!("to the {}", repositories.module_display(&participant.owner));
                let attributes = crate::module_extension::bind_tag_where(
                    schema,
                    &tag.name,
                    &tag.attributes,
                    |text| {
                        let mapping = repositories.mapping(&participant.owner)?;
                        resolution_budget
                            .borrow_mut()
                            .bytes(Label::resolution_size_bound(
                                text,
                                &participant.owner,
                                "",
                                mapping,
                            ))?;
                        let label = repositories.parse(text, &participant.owner, "")?;
                        // LabelConverter with the static recorder
                        // (StarlarkBazelModule.java:117-121).
                        if let Some(apparent) = crate::loading::apparent_repository(text) {
                            recorded
                                .borrow_mut()
                                .push(crate::loading::RecordedRepoMapping {
                                    from: participant.owner.clone(),
                                    apparent: apparent.to_owned(),
                                    canonical: (!label.repository.starts_with('['))
                                        .then(|| label.repository.clone()),
                                });
                        }
                        Ok(label)
                    },
                    &repo_mapping_where,
                    &[],
                )
                .map_err(|error| {
                    format!(
                        "MODULE owner {:?}, tag {:?}: {error}",
                        participant.owner, tag.name
                    )
                })?;
                tags.get_mut(&tag.name)
                    .expect("schema class initialized")
                    .push(BoundTag {
                        attributes,
                        dev_dependency: tag.dev_dependency,
                        ordinal: tag.ordinal,
                    });
            }
            Ok(BoundModule {
                owner: participant.owner.clone(),
                name: participant.name.clone(),
                version: participant.version.clone(),
                is_root: participant.is_root,
                tags,
            })
        })
        .collect()
}

fn alloc_attrs<'v>(
    heap: Heap<'v>,
    attrs: &BTreeMap<String, AttrValue>,
) -> anyhow::Result<Value<'v>> {
    let mut fields = BTreeMap::new();
    for (name, attr) in attrs {
        let value = match attr {
            AttrValue::String(value) => heap.alloc(value.as_str()),
            AttrValue::Strings(values) => heap.alloc(values.clone()),
            AttrValue::StringDict(entries) => {
                ensure!(
                    entries
                        .iter()
                        .all(|(key, value)| key.is_ascii() && value.is_ascii()),
                    "tag string attributes must be ASCII"
                );
                crate::attribute_values::alloc_string_dict(heap, entries)?
            }
            AttrValue::StringListDict(entries) => {
                crate::attribute_values::alloc_string_list_dict(heap, entries)?
            }
            AttrValue::Int(value) => heap.alloc(*value),
            AttrValue::LabelKeyedStringDict(entries) => {
                crate::attribute_values::alloc_label_keyed_string_dict(heap, entries)?
            }
            AttrValue::Ints(values) => heap.alloc(values.clone()),
            AttrValue::StringKeyedLabelDict(entries) => {
                crate::attribute_values::alloc_string_keyed_label_dict(heap, entries)?
            }
            AttrValue::LabelListDict(entries) => {
                crate::attribute_values::alloc_label_dict(heap, entries, true)?
            }
            AttrValue::Bool(value) => heap.alloc(*value),
            AttrValue::Label(None) => Value::new_none(),
            AttrValue::Label(Some(label)) => alloc_label(heap, label)?,
            AttrValue::Labels(labels) => heap.alloc(
                labels
                    .iter()
                    .map(|label| alloc_label(heap, label))
                    .collect::<anyhow::Result<Vec<_>>>()?,
            ),
            AttrValue::Output(_) | AttrValue::Outputs(_) => {
                bail!("output tag attributes are unsupported")
            }
        };
        fields.insert(name.clone(), value);
    }
    Ok(heap.alloc(AllocStruct(fields)))
}

fn projection(modules: &[BoundModule]) -> anyhow::Result<OwnedFrozenValue> {
    Module::with_temp_heap(|module| {
        let heap = module.heap();
        let mut views = Vec::new();
        for (module_index, bound) in modules.iter().enumerate() {
            // StarlarkBazelModule.create (java:115-152): a tag's index is its
            // position among the module's tags for this extension, in MODULE
            // order across tag classes.
            let mut ordinals: Vec<usize> = bound
                .tags
                .values()
                .flatten()
                .map(|tag| tag.ordinal)
                .collect();
            ordinals.sort_unstable();
            let mut classes = BTreeMap::new();
            for (name, tags) in &bound.tags {
                let mut views = Vec::new();
                for tag in tags {
                    let tag_index = ordinals.partition_point(|ordinal| *ordinal < tag.ordinal);
                    views.push(heap.alloc(TagGen {
                        attrs: alloc_attrs(heap, &tag.attributes)?,
                        dev_dependency: tag.dev_dependency,
                        module_index: u32::try_from(module_index)?,
                        tag_index: u32::try_from(tag_index)?,
                    }));
                }
                classes.insert(name.clone(), heap.alloc(views));
            }
            views.push(heap.alloc(BazelModuleGen {
                name: bound.name.clone(),
                version: bound.version.clone(),
                is_root: bound.is_root,
                tags: heap.alloc(TagsGen {
                    classes: heap.alloc(AllocStruct(classes)),
                }),
            }));
        }
        module.set("modules", heap.alloc(views));
        let frozen = module
            .freeze_named(FrozenHeapName::User(Box::new(
                "ordinary extension projection".to_owned(),
            )))
            .map_err(|error| anyhow!("cannot freeze module extension projection: {error:?}"))?;
        // This owner is transferred into the callback heap before it is dropped.
        // All nested lists, including attribute lists and empty classes, freeze.
        frozen
            .get("modules")
            .map_err(|error| anyhow!(error.to_string()))
    })
}

/// Run the entire callback before returning any declarations. No generated
/// repository is bound, mapped or materialized during this execution phase.
/// The module extension's working directory, `<cache>/modextwd/<unique
/// name>` (Bazel: `<output_base>/modextwd/<unique name>`,
/// RegularRunnableExtension.java:364-368). It is created on first use and
/// deleted afterwards by the shared context (repository_ctx::Context).
fn working_directory(repositories: &RepositorySet, unique_name: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        !unique_name.is_empty()
            && !unique_name.contains('/')
            && unique_name != "."
            && unique_name != "..",
        "invalid module extension unique name {unique_name:?}"
    );
    Ok(repositories
        .source_cache_path()
        .map_err(anyhow::Error::msg)?
        .join("modextwd")
        .join(unique_name))
}

#[allow(clippy::too_many_arguments)]
pub fn execute(
    repositories: Arc<RepositorySet>,
    definition: &ExtensionDefinition,
    functions: &[OwnedFrozenValue],
    public_label: &Label,
    extension_name: &str,
    unique_name: &str,
    participants: &[OrdinaryParticipant],
    root_has_non_dev_dependency: bool,
    facts: Vec<(String, crate::lockfile::Json)>,
) -> Result<ExtensionExecution, String> {
    // `Fetching module extension …` in the progress display (when on).
    let _fetching = crate::progress::fetching_extension(|| {
        format!("{}%{extension_name}", public_label.canonical_starlark())
    });
    let run = || -> anyhow::Result<ExtensionExecution> {
        let tag_repo_mappings = RefCell::new(Vec::new());
        let modules = bind_modules(&repositories, definition, participants, &tag_repo_mappings)
            .map_err(anyhow::Error::msg)?;
        let projected = projection(&modules)?;
        let working = working_directory(&repositories, unique_name)?;
        let base = crate::repository_ctx::Context::module_extension(
            repositories.clone(),
            &working,
            public_label.clone(),
            format!("{}%{extension_name}", public_label.canonical_starlark()),
        )?;
        let context = Context {
            repositories,
            public_label: public_label.clone(),
            calls: CallRecorder::default(),
            base,
        };
        for name in definition.environ.names() {
            context
                .base
                .environment
                .borrow_mut()
                .observe(context.repositories.environment(), name)?;
        }
        let (frozen, metadata) = Module::with_temp_heap(|module| -> anyhow::Result<_> {
            let heap = module.heap();
            let ctx = heap.alloc(ModuleCtxGen {
                modules: heap.access_owned_frozen_value(&projected),
                // module_ctx.os.environ is the same repoEnv (StarlarkBaseExternalContext
                // .java:194-196); reading it records no dependency.
                os: {
                    let host = context.repositories.host().map_err(anyhow::Error::msg)?;
                    match context.repositories.environment().client_environment() {
                        Ok(environment) => {
                            crate::host_os::alloc_with_environ(heap, host, environment)?
                        }
                        Err(error) => {
                            crate::host_os::alloc_with_environ_error(heap, host, error.to_string())?
                        }
                    }
                },
                facts: heap.alloc(values::Facts {
                    members: facts.clone(),
                }),
                root_has_non_dev_dependency,
            });
            for function in functions {
                heap.access_owned_frozen_value(function);
            }
            let implementation = heap.access_owned_frozen_value(&definition.implementation);
            let mut eval = Evaluator::new(&module);
            // Bazel bounds neither the heap nor the number of steps of a
            // module extension; only the call depth is bounded here.
            eval.set_max_callstack_size(128)?;
            eval.set_print_handler(&crate::ui::NotePrints);
            eval.extra = Some(&context);
            let evaluated = {
                let _model = starlark::values::string::enter_latin1_char_model(true);
                eval.eval_function(implementation, &[ctx], &[])
            };
            // The callback's non-blocking downloads finish within it, also
            // when it fails (their warnings precede its error).
            crate::repository_ctx::flush_pending_downloads();
            let returned = evaluated.map_err(crate::repository_ctx::callback_error)?;
            // RegularRunnableExtension.java:298-318: None is DEFAULT metadata.
            ensure!(
                returned.is_none() || returned.downcast_ref::<ExtensionMetadata>().is_some(),
                "expected module extension {}%{extension_name} to return None or extension_metadata, got {}",
                public_label.canonical_starlark(),
                returned.get_type()
            );
            let metadata = returned
                .downcast_ref::<ExtensionMetadata>()
                .map(|value| value.metadata.clone());
            drop(eval);
            let frozen = module
                .freeze_named(FrozenHeapName::User(Box::new(format!(
                    "ordinary extension callback:{public_label}"
                ))))
                .map_err(|error| anyhow!("cannot freeze module extension callback: {error:?}"))?;
            Ok((frozen, metadata))
        })?;
        let Context { calls, base, .. } = context;
        let repositories = calls.finish(&frozen).map_err(anyhow::Error::msg)?;
        let inputs = base.finish()?;
        Ok(ExtensionExecution {
            repositories,
            modules,
            metadata,
            inputs,
            tag_repo_mappings: tag_repo_mappings.into_inner(),
        })
    };
    run().map_err(|error| format!("module extension {public_label}: {error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bzlmod::ExtensionTag;
    use crate::extension_calls::RecordedValue;
    use crate::loading::{LoadedDefinition, load_definition_in};
    use serde_json::json;
    use std::fs;

    struct Fixture {
        root: tempfile::TempDir,
        repositories: Arc<RepositorySet>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
            fs::write(root.path().join("BUILD"), "fail('BUILD must not run')").unwrap();
            fs::write(root.path().join("MODULE.bazel"), "module(name='root')").unwrap();
            let repositories = Arc::new(
                RepositorySet::new(root.path().to_owned())
                    .with_source_cache(root.path().join("cache")),
            );
            Self { root, repositories }
        }

        fn file(&self, name: &str, source: &str) {
            let path = self.root.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }

        fn load_source(&self, source: &str) -> LoadedDefinition {
            self.file("defs.bzl", source);
            load_definition_in(
                self.repositories.clone(),
                &label(),
                "ext",
                &crate::package::bzl_globals(),
            )
            .unwrap()
        }

        fn load(&self, body: &str, classes: &str) -> LoadedDefinition {
            let body = body
                .lines()
                .map(|line| format!("    {line}\n"))
                .collect::<String>();
            self.load_source(&format!("def _repo(ctx):\n    fail('repository implementation must not run')\nr = repository_rule(implementation=_repo, attrs={{'words': attr.string_list()}})\ndef _impl(ctx):\n{body}\next = module_extension(implementation=_impl, tag_classes={classes})\n"))
        }

        fn run_loaded(
            &self,
            loaded: LoadedDefinition,
            participants: &[OrdinaryParticipant],
            non_dev: bool,
        ) -> Result<ExtensionExecution, String> {
            let definition = crate::module_extension::unpack(&loaded.value).unwrap();
            let functions = loaded.functions;
            drop(loaded.value);
            drop(loaded.sources);
            execute(
                self.repositories.clone(),
                &definition,
                &functions,
                &label(),
                "ext",
                "_main+ext",
                participants,
                non_dev,
                Vec::new(),
            )
        }

        fn run(
            &self,
            body: &str,
            classes: &str,
            participants: &[OrdinaryParticipant],
        ) -> Result<ExtensionExecution, String> {
            self.run_loaded(self.load(body, classes), participants, true)
        }

        fn error(
            &self,
            body: &str,
            classes: &str,
            participants: &[OrdinaryParticipant],
            expected: &str,
        ) {
            let error = self.run(body, classes, participants).unwrap_err();
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
    }

    fn label() -> Label {
        Label::parse_canonical("//:defs.bzl").unwrap()
    }

    fn participant(tags: Vec<ExtensionTag>) -> OrdinaryParticipant {
        OrdinaryParticipant {
            owner: String::new(),
            name: "root".to_owned(),
            version: "1.2".to_owned(),
            is_root: true,
            tags,
            imports: Vec::new(),
            source_profile_error: None,
            module_key: "<root>".to_owned(),
            extension_bzl_file: "@root//:defs.bzl".to_owned(),
        }
    }

    fn tag(
        name: &str,
        ordinal: usize,
        attrs: serde_json::Value,
        dev_dependency: bool,
    ) -> ExtensionTag {
        ExtensionTag {
            name: name.to_owned(),
            ordinal,
            attributes: attrs
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            attribute_order: attrs.as_object().unwrap().keys().cloned().collect(),
            declaration: 0,
            dev_dependency,
        }
    }

    #[test]
    fn repeated_tag_and_repository_defaults_have_no_native_host_limit() {
        // Bazel bounds no extension input or result; the former 64 MiB
        // weighted host budget rejected 600 tags/calls with 1 MiB defaults.
        let fixture = Fixture::new();
        let classes = "{'t':tag_class({'value':attr.string(default='x' * 1048576)})}";
        let many = [participant(
            (0..600).map(|i| tag("t", i, json!({}), false)).collect(),
        )];
        fixture.error(
            "len(ctx.modules[0].tags.t) == 600 or fail('count')\nfail('CALLBACK_REACHED')",
            classes,
            &many,
            "CALLBACK_REACHED",
        );
        assert!(
            !fixture
                .root
                .path()
                .join("cache/modextwd/_main+ext")
                .exists()
        );
        for count in [1, 600] {
            let loaded = fixture.load_source(&format!(
                "def _repo(ctx):\n    fail('FETCH_REACHED')\nr = repository_rule(_repo, attrs={{'value':attr.string(default='x' * 1048576)}})\ndef _impl(ctx):\n    for i in range({count}):\n        r(name='made' + str(i))\n    fail('CALLBACK_COMPLETED')\next = module_extension(_impl)\n"
            ));
            let error = fixture.run_loaded(loaded, &[], true).unwrap_err();
            assert!(error.contains("CALLBACK_COMPLETED"), "{error}");
        }
    }

    #[test]
    fn recording_defers_binding_of_unknown_and_missing_attributes() {
        let fixture = Fixture::new();
        // Cost estimation must not eagerly bind unknown/missing mandatory attrs.
        let loaded = fixture.load_source("def _repo(ctx):\n    pass\nr = repository_rule(_repo, attrs={'required':attr.string(mandatory=True)})\ndef _impl(ctx):\n    r(name='made', unknown=True)\n    fail('DEFERRED_BINDING')\next = module_extension(_impl)\n");
        let error = fixture.run_loaded(loaded, &[], true).unwrap_err();
        assert!(error.contains("DEFERRED_BINDING"), "{error}");
        assert!(
            !fixture
                .root
                .path()
                .join("cache/modextwd/_main+ext")
                .exists()
        );
    }

    #[test]
    fn native_tags_use_exact_schema_fields_and_real_labels() {
        let fixture = Fixture::new();
        let participants = [participant(vec![tag(
            "t",
            4,
            json!({"_private":"yes", "name":"declared", "src":":supplied", "unknown":null}),
            true,
        )])];
        let result = fixture.run(r#"
m = ctx.modules[0]
t = m.tags.t[0]
(type(ctx) == 'module_ctx' and type(m) == 'bazel_module' and type(t) == 'bazel_module_tag' and type(m.tags) == 'bazel_module_tags' and repr(m.tags) == '<unknown object com.google.devtools.build.lib.bazel.bzlmod.StarlarkBazelModule$Tags>') or fail('types')
(m.name == 'root' and m.version == '1.2' and m.is_root) or fail('module')
(ctx.root_module_has_non_dev_dependency == True) or fail('root flag')
(t._private == 'yes' and t.name == 'declared' and t.dev_dependency == 7) or fail('schema')
ctx.is_dev_dependency(t) or fail('hidden dev flag')
(type(t.src) == 'Label' and t.src.name == 'supplied') or fail('label')
(type(t.srcs[0]) == 'Label' and t.srcs[0].package == 'defaults') or fail('labels')
(t.optional == None and t.words == ['before'] and t.flag and t.count == -7) or fail('defaults')
(sorted(dir(t)) == ['_private', 'count', 'dev_dependency', 'flag', 'name', 'optional', 'src', 'srcs', 'words']) or fail('fields')
"#, "{'t': tag_class({'_private': attr.string(mandatory=True), 'name': attr.string(), 'dev_dependency': attr.int(default=7), 'src': attr.label(), 'srcs': attr.label_list(default=['//defaults:one']), 'optional': attr.label(), 'words': attr.string_list(default=['before']), 'flag': attr.bool(default=True), 'count': attr.int(default=-7)})}", &participants).unwrap();
        assert!(result.repositories.is_empty());
        assert_eq!(result.modules[0].tags["t"][0].ordinal, 4);
        assert_eq!(
            result.modules[0].tags["t"][0].attributes["src"],
            AttrValue::Label(Some("//:supplied".to_owned()))
        );
    }

    #[test]
    fn string_dict_tag_projection_is_ordered_scalar_and_frozen() {
        let fixture = Fixture::new();
        let participants = [participant(vec![tag(
            "t",
            0,
            json!({"d":{"z":"@missing//p:t", "a":""}}),
            false,
        )])];
        let schema = "{'t':tag_class({'d':attr.string_dict(allow_empty=False)})}";
        fixture.run("d=ctx.modules[0].tags.t[0].d\n(d.keys() == ['z','a'] and d.values() == ['@missing//p:t',''] and type(d['z']) == 'string') or fail('projection')", schema, &participants).unwrap();
        let error = fixture
            .run(
                "ctx.modules[0].tags.t[0].d['z']='changed'",
                schema,
                &participants,
            )
            .unwrap_err()
            .to_lowercase();
        assert!(
            error.contains("frozen") || error.contains("immutable"),
            "{error}"
        );
        Module::with_temp_heap(|module| {
            for (key, value) in [("é", "ok"), ("k", "é")] {
                let attrs = BTreeMap::from([(
                    "d".into(),
                    AttrValue::StringDict(vec![(key.into(), value.into())]),
                )]);
                assert!(
                    alloc_attrs(module.heap(), &attrs)
                        .unwrap_err()
                        .to_string()
                        .contains("ASCII")
                );
            }
        });
    }

    #[test]
    fn participant_order_zero_tags_empty_classes_and_global_ordinals() {
        let fixture = Fixture::new();
        let mut dep = participant(Vec::new());
        dep.owner = "dep+".to_owned();
        dep.name = "dep".to_owned();
        dep.version = "3.4".to_owned();
        dep.is_root = false;
        let participants = [
            participant(vec![
                tag("t", 9, json!({"value":"later"}), false),
                tag("other", 5, json!({}), false),
                tag("t", 2, json!({"value":"first"}), true),
            ]),
            dep,
        ];
        let loaded = fixture.load(r#"
([m.name for m in ctx.modules] == ['root', 'dep']) or fail('order')
([m.version for m in ctx.modules] == ['1.2', '3.4']) or fail('versions')
(not ctx.modules[1].is_root and not ctx.root_module_has_non_dev_dependency) or fail('flags')
([t.value for t in ctx.modules[0].tags.t] == ['first', 'later']) or fail('tag order')
(ctx.modules[1].tags.t == [] and ctx.modules[1].tags.other == [] and ctx.modules[0].tags.empty == []) or fail('empty classes')
(sorted(dir(ctx.modules[0])) == ['is_root', 'name', 'tags', 'version']) or fail('module fields')
"#, "{'t': tag_class({'value': attr.string()}), 'other': tag_class(), 'empty': tag_class()}");
        let result = fixture.run_loaded(loaded, &participants, false).unwrap();
        assert_eq!(
            result.modules[0].tags["t"]
                .iter()
                .map(|t| t.ordinal)
                .collect::<Vec<_>>(),
            [2, 9]
        );
        assert_eq!(result.modules[1].owner, "dep+");
    }

    #[test]
    fn invalid_tags_fail_before_callback_and_none_is_omission() {
        let fixture = Fixture::new();
        for (tags, expected) in [
            (
                vec![tag("unknown", 0, json!({}), false)],
                "unknown tag class",
            ),
            (
                vec![tag("t", 0, json!({"required":null}), false)],
                "mandatory",
            ),
            (
                vec![tag("t", 0, json!({"required":"ok", "unknown":1}), false)],
                "unknown tag attribute",
            ),
            (
                vec![tag("t", 0, json!({"required":"é"}), false)],
                "non-ASCII",
            ),
        ] {
            fixture.error(
                "fail('callback ran')",
                "{'t':tag_class({'required':attr.string(mandatory=True)})}",
                &[participant(tags)],
                expected,
            );
        }
        fixture
            .run(
                "(ctx.modules[0].tags.t[0].value == 'fallback') or fail('None')",
                "{'t':tag_class({'value':attr.string(default='fallback')})}",
                &[participant(vec![tag(
                    "t",
                    0,
                    json!({"value":null, "unknown":null}),
                    false,
                )])],
            )
            .unwrap();
        for field in ["name", "dev_dependency", "ordinal"] {
            fixture.error(
                &format!("ctx.modules[0].tags.t[0].{field}"),
                "{'t':tag_class()}",
                &[participant(vec![tag("t", 0, json!({}), false)])],
                field,
            );
        }
    }

    #[test]
    fn projection_and_all_nested_lists_are_frozen_before_callback() {
        let fixture = Fixture::new();
        let participants = [participant(vec![tag("t", 0, json!({}), false)])];
        let classes = "{'t':tag_class({'words':attr.string_list(default=['before']), 'labels':attr.label_list(default=['//:x'])}), 'empty':tag_class()}";
        for expression in [
            "ctx.modules",
            "ctx.modules[0].tags.t",
            "ctx.modules[0].tags.t[0].words",
            "ctx.modules[0].tags.t[0].labels",
            "ctx.modules[0].tags.empty",
        ] {
            fixture.error(
                &format!("{expression}.append(None)"),
                classes,
                &participants,
                "trying to mutate a frozen list value",
            );
        }
        // Bazel 9.3 EvalUtils.setField / Structure.setField texts.
        for (assignment, expected) in [
            (
                "ctx.modules = []",
                "cannot set .modules field of module_ctx value",
            ),
            (
                "ctx.modules[0].name = 'changed'",
                "cannot set .name field of bazel_module value",
            ),
            (
                "ctx.modules[0].tags.t = []",
                "bazel_module_tags value does not support field assignment",
            ),
            (
                "ctx.modules[0].tags.t[0].words = []",
                "bazel_module_tag value does not support field assignment",
            ),
        ] {
            fixture.error(assignment, classes, &participants, expected);
        }
        fixture
            .run(
                "(ctx.modules[0].tags.t[0].words == ['before']) or fail('changed')",
                classes,
                &participants,
            )
            .unwrap();
    }

    #[test]
    fn dev_helper_requires_genuine_tag_and_positional_argument() {
        let fixture = Fixture::new();
        let participants = [participant(vec![tag("t", 0, json!({}), false)])];
        let classes = "{'t':tag_class()}";
        fixture
            .run(
                "(not ctx.is_dev_dependency(ctx.modules[0].tags.t[0])) or fail('dev')",
                classes,
                &participants,
            )
            .unwrap();
        for value in [
            "struct(dev_dependency=True)",
            "{}",
            "ctx.modules[0]",
            "True",
            "None",
        ] {
            fixture.error(
                &format!("ctx.is_dev_dependency({value})"),
                classes,
                &participants,
                "expects a tag",
            );
        }
        fixture.error(
            "ctx.is_dev_dependency(tag=ctx.modules[0].tags.t[0])",
            classes,
            &participants,
            "tag",
        );
        // A bool struct field, not a method (Starlark.java:904).
        fixture.error(
            "ctx.root_module_has_non_dev_dependency()",
            classes,
            &participants,
            "'bool' object is not callable",
        );
    }

    #[test]
    fn dev_helper_accepts_genuine_tag_from_another_projection() {
        use starlark::syntax::{AstModule, Dialect};
        let foreign = projection(&[BoundModule {
            owner: "foreign+".to_owned(),
            name: "foreign".to_owned(),
            version: String::new(),
            is_root: false,
            tags: BTreeMap::from([(
                "t".to_owned(),
                vec![BoundTag {
                    attributes: BTreeMap::new(),
                    dev_dependency: true,
                    ordinal: 0,
                }],
            )]),
        }])
        .unwrap();
        let own = projection(&[]).unwrap();
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            module.set("foreign", heap.access_owned_frozen_value(&foreign));
            module.set(
                "ctx",
                heap.alloc(ModuleCtxGen {
                    modules: heap.access_owned_frozen_value(&own),
                    os: crate::host_os::alloc(
                        heap,
                        crate::host_os::HostSnapshot::default().get().unwrap(),
                    )
                    .unwrap(),
                    facts: heap.alloc(values::Facts {
                        members: Vec::new(),
                    }),
                    root_has_non_dev_dependency: false,
                }),
            );
            let source = "ctx.is_dev_dependency(foreign[0].tags.t[0]) or fail('foreign genuine tag rejected')";
            Evaluator::new(&module)
                .eval_module(
                    AstModule::parse("test.star", source.to_owned(), &Dialect::Standard).unwrap(),
                    &crate::package::bzl_globals(),
                )
                .unwrap();
        });
    }

    #[test]
    fn records_real_calls_computed_names_and_call_time_argument_snapshots() {
        let fixture = Fixture::new();
        let result = fixture
            .run(
                r#"
words = ['before']
for suffix in ['one', 'two']:
    r(name='computed_' + suffix, words=words, src=Label('//:literal'), deferred='@later//:target')
words.append('after')
"#,
                "{}",
                &[],
            )
            .unwrap();
        assert_eq!(
            result
                .repositories
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["computed_one", "computed_two"]
        );
        for record in result.repositories {
            assert_eq!(record.definition.identity, "//:defs.bzl%r");
            assert_eq!(
                record.attributes["words"],
                RecordedValue::List(vec![RecordedValue::String("before".to_owned())])
            );
            assert_eq!(
                record.attributes["src"],
                RecordedValue::Label("//:literal".to_owned())
            );
            assert_eq!(
                record.attributes["deferred"],
                RecordedValue::String("@later//:target".to_owned())
            );
        }
        assert!(
            !fixture
                .root
                .path()
                .join("cache/modextwd/_main+ext")
                .exists()
        );
    }

    #[test]
    fn failure_invalid_return_and_duplicate_calls_never_return_partial_results() {
        let fixture = Fixture::new();
        fixture.error(
            "r(name='first')\nfail('late failure')",
            "{}",
            &[],
            "late failure",
        );
        for (returned, kind) in [
            ("42", "int"),
            ("{}", "dict"),
            ("struct(root_module_direct_deps=[])", "struct"),
            ("ctx.facts", "Facts"),
        ] {
            fixture.error(
                &format!("r(name='first')\nreturn {returned}"),
                "{}",
                &[],
                &format!("expected module extension @@//:defs.bzl%ext to return None or extension_metadata, got {kind}"),
            );
        }
        fixture.error(
            "r(name='first')\nr(name='first')\nfail('too late')",
            "{}",
            &[],
            "duplicate generated repository name",
        );
        fixture.error("r('positional')", "{}", &[], "positional");
        fixture.error("r(name='bad+name')", "{}", &[], "name");
        assert!(
            !fixture
                .root
                .path()
                .join("cache/modextwd/_main+ext")
                .exists()
        );
    }

    #[test]
    fn module_ctx_members_are_bazels_base_and_extension_members_only() {
        let fixture = Fixture::new();
        // dir(module_ctx) of Bazel 9.3: StarlarkBaseExternalContext members
        // (load_wasm/execute_wasm need --experimental_repository_ctx_execute_wasm)
        // and ModuleExtensionContext members (is_isolated needs
        // --experimental_isolated_extension_usages).
        fixture
            .run(
                "expected = ['download', 'download_and_extract', 'execute', 'extension_metadata', 'extract', 'facts', 'file', 'getenv', 'is_dev_dependency', 'modules', 'os', 'path', 'read', 'report_progress', 'root_module_has_non_dev_dependency', 'tag_sort_key', 'watch', 'which']\nsorted(dir(ctx)) == expected or fail(sorted(dir(ctx)))",
                "{}",
                &[],
            )
            .unwrap();
        for field in [
            "delete",
            "symlink",
            "template",
            "rename",
            "patch",
            "watch_tree",
            "repo_metadata",
            "attr",
            "name",
            "is_isolated",
            "load_wasm",
        ] {
            fixture.error(&format!("ctx.{field}"), "{}", &[], field);
        }
        fixture.error(
            "ctx.modules[0].repo_name",
            "{}",
            &[participant(Vec::new())],
            "repo_name",
        );
    }

    #[test]
    fn json_unicode_escapes_in_extension_callback_are_bazel_byte_strings() {
        let fixture = Fixture::new();
        fixture
            .run(
                r#"
value = json.decode('"\\u00e9"')
len(value) == 2 or fail('expected two UTF-8 byte chars, got %r' % value)
json.decode('"\\u0041"') == 'A' or fail('ascii escape changed')
"#,
                "{}",
                &[],
            )
            .unwrap();
    }

    fn files(result: &ExtensionExecution) -> Vec<serde_json::Value> {
        result
            .inputs
            .files
            .iter()
            .map(|o| serde_json::to_value(o).unwrap())
            .collect()
    }

    #[test]
    fn module_path_and_read_resolve_labels_and_watch_like_bazel() {
        let fixture = Fixture::new();
        let bytes = b"module example.com/app\n\ngo 1.27.1\n// \xe9\x00\n";
        fs::write(fixture.root.path().join("go.mod"), bytes).unwrap();
        fixture.file("sub/BUILD.bazel", "");
        fixture.file("sub/nested/data.txt", "nested");
        std::os::unix::fs::symlink("go.mod", fixture.root.path().join("link")).unwrap();
        let root = fs::canonicalize(fixture.root.path()).unwrap();
        let expected = root.join("go.mod").to_str().unwrap().to_owned();
        let nested = root
            .join("sub/nested/data.txt")
            .to_str()
            .unwrap()
            .to_owned();
        let result = fixture
            .run(
                &format!(
                    r#"
p = ctx.path(Label('//:go.mod'))
type(p) == 'path' or fail(type(p))
str(p) == {expected:?} or fail(str(p))
repr(p) == repr({expected:?}) or fail(repr(p))
ctx.path(p) == p or fail('path passthrough')
{{p: 1}}[ctx.path(Label('//:go.mod'))] == 1 or fail('path hash')
p.basename == 'go.mod' or fail(p.basename)
p.dirname.get_child('go.mod') == p or fail('dirname/get_child')
p.exists and not p.is_dir and p.dirname.is_dir or fail('exists/is_dir')
sorted(dir(p)) == ['basename', 'dirname', 'exists', 'get_child', 'is_dir', 'readdir', 'realpath'] or fail(dir(p))
ctx.path(Label('//:link')).realpath == p or fail('realpath')
'go.mod' in [e.basename for e in p.dirname.readdir()] or fail('readdir')
str(ctx.path(Label('//sub:nested/data.txt'))) == {nested:?} or fail('nested')
not ctx.path(Label('//:never_created')).exists or fail('missing files resolve')
text = ctx.read(p)
len(text) == {size} or fail(len(text))
text[-3:] == '\351\000\n' or fail('not Latin-1 byte chars')
text == ctx.read(Label('//:go.mod'), watch='yes') or fail('label read')
text == ctx.read(p, watch='no') or fail('watch no')
text == ctx.read(Label('//:link')) or fail('symlinks are followed')
ctx.read(Label('//sub:nested/data.txt')) == 'nested' or fail('nested read')
version = [l for l in text.split('\n') if l.startswith('go ')][0][3:]
r(name='sdk', words=[version])
"#,
                    size = bytes.len()
                ),
                "{}",
                &[],
            )
            .unwrap();
        assert_eq!(
            result.repositories[0].attributes["words"],
            RecordedValue::List(vec![RecordedValue::String("1.27.1".to_owned())])
        );
        let sha = crate::extension_download::sha256(bytes);
        let observed = files(&result);
        // The workspace directory is watched by readdir(watch='auto'); the
        // Label reads record the canonical label; watch='no' records nothing.
        assert!(observed.contains(
            &json!({"path": "//:go.mod", "kind": "file", "state": format!("file:{sha}")})
        ));
        assert!(
            observed.contains(&json!({"path": "//sub:nested/data.txt", "kind": "file",
            "state": format!("file:{}", crate::extension_download::sha256(b"nested"))}))
        );
        assert!(
            observed
                .iter()
                .any(|o| o["path"] == "@@//" && o["kind"] == "dirents")
        );
        // The working directory is removed afterwards.
        assert!(
            !fixture
                .root
                .path()
                .join("cache/modextwd/_main+ext")
                .exists()
        );
    }

    #[test]
    fn module_ctx_working_directory_file_io_execute_and_watch_restrictions() {
        let fixture = Fixture::new();
        fixture.file("in.txt", "input");
        let working = fs::canonicalize(fixture.root.path())
            .unwrap()
            .join("cache/modextwd/_main+ext");
        let result = fixture
            .run(
                &format!(
                    r#"
wd = ctx.path('.')
str(wd) == {working:?} or fail(str(wd))
str(ctx.path('a/b')) == {working:?} + '/a/b' or fail('relative')
ctx.file('sub/f.txt', 'hello\n', executable=False)
ctx.read('sub/f.txt') == 'hello\n' or fail('read back')
ctx.path('sub/f.txt').exists or fail('exists')
[p.basename for p in ctx.path('sub').readdir()] == ['f.txt'] or fail('readdir wd')
res = ctx.execute(['sh', '-c', 'pwd; cat sub/f.txt; echo err >&2; exit 3'])
type(res) == 'exec_result' or fail(type(res))
res.return_code == 3 and res.stdout == {working:?} + '\nhello\n' and res.stderr == 'err\n' or fail(res.stdout)
ctx.execute(['sh', '-c', 'pwd'], working_directory='sub').stdout == {working:?} + '/sub\n' or fail('working_directory')
ctx.execute(['sh', '-c', 'echo $X'], environment={{'X': 'y'}}, quiet=True, timeout=5).stdout == 'y\n' or fail('environment')
ctx.which('sh') != None or fail('which')
ctx.which('definitely-not-a-program-n43') == None or fail('which missing')
ctx.report_progress('working')
ctx.watch(Label('//:in.txt'))
ctx.read(Label('//:in.txt')) == 'input' or fail('label read')
ctx.read(ctx.path('sub/f.txt'), watch='auto')
"#
                ),
                "{}",
                &[],
            )
            .unwrap();
        assert!(!working.exists(), "working directory must be deleted");
        let observed = files(&result);
        assert!(observed.iter().all(|o| o["path"] != "sub/f.txt"));
        assert!(
            observed.contains(&json!({"path": "//:in.txt", "kind": "file",
            "state": format!("file:{}", crate::extension_download::sha256(b"input"))}))
        );
        assert!(result.inputs.host_facilities.contains("execute"));
        assert!(result.inputs.host_facilities.contains("which"));
        for (body, expected) in [
            (
                "ctx.watch('x')",
                "attempted to watch path under working directory",
            ),
            (
                "ctx.read(ctx.path('/etc/hostname'), watch='yes')",
                "attempted to watch path outside workspace, but it's prohibited in the current context",
            ),
            (
                "ctx.watch('/etc/hostname')",
                "attempted to watch path outside workspace, but it's prohibited in the current context",
            ),
            (
                "ctx.file('/tmp/opencode/n43-never-written', '')",
                "Cannot write outside of the repository directory for path /tmp/opencode/n43-never-written",
            ),
            ("ctx.read('missing')", "No such file or directory"),
            (
                "ctx.path(1)",
                "in call to path(), parameter 'path' got value of type 'int', want 'string, Label, or path'",
            ),
            (
                "ctx.read(Label('//missing:go.mod'))",
                "Unable to load package for //missing:go.mod: ",
            ),
            (
                "ctx.read('x', watch='bad')",
                "bad value for 'watch' parameter; want 'yes', 'no', or 'auto', got bad",
            ),
        ] {
            fixture.error(body, "{}", &[], expected);
            assert!(!working.exists(), "working directory must be deleted");
        }
        // watch='auto' of a host path outside the workspace silently skips.
        let result = fixture
            .run("ctx.read(ctx.path('/etc/hostname'))", "{}", &[])
            .unwrap();
        assert!(result.inputs.files.is_empty());
        assert!(!std::path::Path::new("/tmp/opencode/n43-never-written").exists());
    }

    #[test]
    fn module_ctx_extract_and_download_and_extract_into_the_working_directory() {
        use crate::extension_io::tests::{Server, response};
        let fixture = Fixture::new();
        let archive = {
            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(5);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "top/data.txt", &b"inner"[..])
                .unwrap();
            builder.into_inner().unwrap()
        };
        fs::write(fixture.root.path().join("arc.tar"), &archive).unwrap();
        let sha = crate::extension_download::sha256(&archive);
        let server = Server::new(vec![response(200, &archive, "")]);
        fixture
            .run(
                &format!(
                    r#"
ctx.extract(Label('//:arc.tar'), 'out', strip_prefix='top')
ctx.read('out/data.txt') == 'inner' or fail('extract')
r = ctx.download_and_extract('{url}/arc.tar', 'dl', sha256='{sha}', strip_prefix='top')
r.success and r.sha256 == '{sha}' or fail(r)
ctx.read('dl/data.txt') == 'inner' or fail('download_and_extract')
"#,
                    url = server.url
                ),
                "{}",
                &[],
            )
            .unwrap();
        assert_eq!(server.count(), 1);
    }

    #[test]
    fn module_facts_are_empty_opaque_and_unhashable() {
        let fixture = Fixture::new();
        let result = fixture
            .run(
                r#"
f = ctx.facts
type(f) == 'Facts' or fail(type(f))
repr(f) == 'Facts(<opaque, inspect with print()>)' or fail(repr(f))
str(f) == repr(f) or fail(str(f))
(f.get('k') == None and f.get('k', 3) == 3 and f.get(key='k', default=4) == 4) or fail('get')
('k' not in f and not ('k' in f)) or fail('in')
dir(f) == ['get'] or fail(dir(f))
hasattr(ctx, 'facts') or fail('hasattr')
(ctx.facts == ctx.facts and f == ctx.facts and f != None) or fail('facts equality')
sorted(dir(ctx)) == ['download', 'download_and_extract', 'execute', 'extension_metadata', 'extract', 'facts', 'file', 'getenv', 'is_dev_dependency', 'modules', 'os', 'path', 'read', 'report_progress', 'root_module_has_non_dev_dependency', 'tag_sort_key', 'watch', 'which'] or fail(sorted(dir(ctx)))
dir(ctx) == sorted(dir(ctx)) or fail('dir order')
"#,
                "{}",
                &[],
            )
            .unwrap();
        assert!(result.inputs.files.is_empty());
        for (body, expected) in [
            ("{ctx.facts: 1}", "unhashable type: 'Facts'"),
            ("ctx.facts['k']", "Facts has no key \"k\""),
            ("len(ctx.facts)", "len"),
            ("[x for x in ctx.facts]", "iter"),
            ("ctx.facts.get(1)", "get"),
            ("ctx.facts.keys()", "keys"),
        ] {
            fixture.error(body, "{}", &[], expected);
        }
    }

    #[test]
    fn extension_metadata_validates_facts_and_is_the_only_non_none_return() {
        let fixture = Fixture::new();
        let result = fixture
            .run(
                r#"
r(name='made')
m = ctx.extension_metadata(reproducible=True, facts={'a': [1, 2.5, None, True, ('x',)], 'b': {'c': {'d': 'e'}}, 'deep': [[[[[[1]]]]]]})
type(m) == 'extension_metadata' or fail(type(m))
type(ctx.extension_metadata()) == 'extension_metadata' or fail('default')
type(ctx.extension_metadata(root_module_direct_deps=None, root_module_direct_dev_deps=None, facts={})) == 'extension_metadata' or fail('none')
return m
"#,
                "{}",
                &[],
            )
            .unwrap();
        assert_eq!(result.repositories.len(), 1);
        for (body, expected) in [
            (
                "ctx.extension_metadata(facts=[])",
                "in call to extension_metadata(), parameter 'facts' got value of type 'list', want 'dict'",
            ),
            (
                "ctx.extension_metadata(facts={1: 'x'})",
                "got dict<int, string> for 'facts', want dict<string, unknown>",
            ),
            (
                "ctx.extension_metadata(facts={'a': {'b': {1: 'x'}}})",
                "Facts keys must be strings, got '1: \"x\"' (int)",
            ),
            (
                "ctx.extension_metadata(facts={'a': struct()})",
                "'struct()' (struct) is not supported in facts",
            ),
            (
                "ctx.extension_metadata(facts={'a': [[[[[[[1]]]]]]]})",
                "Facts cannot be nested more than 7 levels deep",
            ),
            (
                "ctx.extension_metadata(root_module_direct_deps=['made'], root_module_direct_dev_deps=[], facts={1: 2})",
                "got dict<int, int> for 'facts'",
            ),
            (
                "ctx.extension_metadata(None)",
                "extension_metadata() got unexpected positional argument",
            ),
            (
                "ctx.extension_metadata(reproducible=1)",
                "in call to extension_metadata(), parameter 'reproducible' got value of type 'int', want 'bool'",
            ),
            (
                "{ctx.extension_metadata(): 1}",
                "unhashable type: 'extension_metadata'",
            ),
            (
                "return [ctx.extension_metadata()]",
                "expected module extension @@//:defs.bzl%ext to return None or extension_metadata, got list",
            ),
        ] {
            fixture.error(body, "{}", &[], expected);
        }
    }

    fn metadata_of(
        fixture: &Fixture,
        body: &str,
    ) -> Option<crate::extension_metadata::ExtensionMetadata> {
        fixture
            .run(body, "{}", &[])
            .unwrap_or_else(|error| panic!("{body}: {error}"))
            .metadata
    }

    #[test]
    fn extension_metadata_direct_dependency_modes_are_carried_out_of_execute() {
        use crate::extension_metadata::{ExtensionMetadata, UseAllRepos};
        let fixture = Fixture::new();
        let mode = |use_all_repos, reproducible| ExtensionMetadata {
            use_all_repos,
            reproducible,
            ..Default::default()
        };
        let explicit = |deps: &[&str], dev: &[&str]| ExtensionMetadata {
            explicit_root_module_direct_deps: Some(deps.iter().map(|s| (*s).to_owned()).collect()),
            explicit_root_module_direct_dev_deps: Some(
                dev.iter().map(|s| (*s).to_owned()).collect(),
            ),
            ..Default::default()
        };
        assert_eq!(metadata_of(&fixture, "r(name='made')"), None);
        assert_eq!(
            metadata_of(
                &fixture,
                "ctx.extension_metadata(root_module_direct_deps='all', root_module_direct_dev_deps=[])"
            ),
            None
        );
        for (body, expected) in [
            (
                "return ctx.extension_metadata()",
                mode(UseAllRepos::No, false),
            ),
            (
                "return ctx.extension_metadata(root_module_direct_deps=None, root_module_direct_dev_deps=None, reproducible=True)",
                mode(UseAllRepos::No, true),
            ),
            (
                "return ctx.extension_metadata(root_module_direct_deps='all', root_module_direct_dev_deps=[], reproducible=True)",
                mode(UseAllRepos::Regular, true),
            ),
            (
                "return ctx.extension_metadata(root_module_direct_dev_deps=[], root_module_direct_deps='all')",
                mode(UseAllRepos::Regular, false),
            ),
            (
                "return ctx.extension_metadata(root_module_direct_deps=[], root_module_direct_dev_deps='all')",
                mode(UseAllRepos::Dev, false),
            ),
            (
                "return ctx.extension_metadata(**{'root_module_direct_deps': 'all', 'root_module_direct_dev_deps': []})",
                mode(UseAllRepos::Regular, false),
            ),
            // bazel_skylib modules.bzl:111-114 use_all_repos (root non-dev flag is True here).
            (
                "h = ctx.root_module_has_non_dev_dependency\nreturn ctx.extension_metadata(root_module_direct_deps='all' if h else [], root_module_direct_dev_deps=[] if h else 'all', **{'reproducible': True})",
                mode(UseAllRepos::Regular, true),
            ),
            (
                "return ctx.extension_metadata(root_module_direct_deps=['b', 'a'], root_module_direct_dev_deps=('c', 'd-1.x_y'))",
                explicit(&["b", "a"], &["c", "d-1.x_y"]),
            ),
            (
                "return ctx.extension_metadata(root_module_direct_deps=[], root_module_direct_dev_deps=[])",
                explicit(&[], &[]),
            ),
            (
                "return ctx.extension_metadata(root_module_direct_deps=range(0), root_module_direct_dev_deps=())",
                explicit(&[], &[]),
            ),
        ] {
            assert_eq!(metadata_of(&fixture, body), Some(expected), "{body}");
        }
        // Dev projection: root_module_has_non_dev_dependency False selects DEV.
        let loaded = fixture.load("h = ctx.root_module_has_non_dev_dependency\nreturn ctx.extension_metadata(root_module_direct_deps='all' if h else [], root_module_direct_dev_deps=[] if h else 'all')", "{}");
        assert_eq!(
            fixture.run_loaded(loaded, &[], false).unwrap().metadata,
            Some(mode(UseAllRepos::Dev, false))
        );
    }

    #[test]
    fn extension_metadata_direct_dependency_validation_matches_bazel_text_and_order() {
        let fixture = Fixture::new();
        let either_all = "if one of root_module_direct_deps and root_module_direct_dev_deps is \"all\", the other must be an empty list";
        let strings = "root_module_direct_deps and root_module_direct_dev_deps must be None, \"all\", or a list of strings";
        let both = "root_module_direct_deps and root_module_direct_dev_deps must both be specified or both be unspecified";
        let repo = "valid names may contain only A-Z, a-z, 0-9, '-', '_', '.', and must start with a letter or a number";
        for (args, expected) in [
            // StarlarkList.equals: a tuple or range never pairs with "all".
            ("root_module_direct_deps='all', root_module_direct_dev_deps=()", either_all.to_owned()),
            ("root_module_direct_deps=(), root_module_direct_dev_deps='all'", either_all.to_owned()),
            ("root_module_direct_deps='all', root_module_direct_dev_deps=range(0)", either_all.to_owned()),
            ("root_module_direct_deps='all', root_module_direct_dev_deps=None", either_all.to_owned()),
            ("root_module_direct_deps='all'", either_all.to_owned()),
            ("root_module_direct_dev_deps='all'", either_all.to_owned()),
            ("root_module_direct_deps='all', root_module_direct_dev_deps='all'", either_all.to_owned()),
            ("root_module_direct_deps='all', root_module_direct_dev_deps=['x']", either_all.to_owned()),
            ("root_module_direct_deps='some', root_module_direct_dev_deps=[]", strings.to_owned()),
            ("root_module_direct_deps=[], root_module_direct_dev_deps='ALL'", strings.to_owned()),
            ("root_module_direct_deps='x'", strings.to_owned()),
            ("root_module_direct_deps=['a']", both.to_owned()),
            ("root_module_direct_dev_deps=()", both.to_owned()),
            ("root_module_direct_deps=1", "in call to extension_metadata(), parameter 'root_module_direct_deps' got value of type 'int', want 'sequence, string, or NoneType'".to_owned()),
            ("root_module_direct_dev_deps={}", "in call to extension_metadata(), parameter 'root_module_direct_dev_deps' got value of type 'dict', want 'sequence, string, or NoneType'".to_owned()),
            ("root_module_direct_deps=True, root_module_direct_dev_deps=[]", "parameter 'root_module_direct_deps' got value of type 'bool'".to_owned()),
            // Named arguments are type-checked in call order.
            ("facts=[], root_module_direct_deps=1", "parameter 'facts' got value of type 'list', want 'dict'".to_owned()),
            ("root_module_direct_deps=1, facts=[]", "parameter 'root_module_direct_deps' got value of type 'int'".to_owned()),
            ("reproducible='yes', root_module_direct_dev_deps=1", "parameter 'reproducible' got value of type 'string', want 'bool'".to_owned()),
            // Facts (Dict.cast + validateAndCreate) precede create()'s checks.
            ("root_module_direct_deps='bogus', facts={'a': struct()}", "'struct()' (struct) is not supported in facts".to_owned()),
            ("root_module_direct_deps=[1], root_module_direct_dev_deps=[]", "at index 0 of root_module_direct_deps, got element of type int, want string".to_owned()),
            ("root_module_direct_deps=['a'], root_module_direct_dev_deps=('b', None)", "at index 1 of root_module_direct_dev_deps, got element of type NoneType, want string".to_owned()),
            ("root_module_direct_deps=range(2), root_module_direct_dev_deps=[]", "at index 0 of root_module_direct_deps, got element of type int, want string".to_owned()),
            // Both casts precede every per-entry repository-name check.
            ("root_module_direct_deps=['a+b'], root_module_direct_dev_deps=[1]", "at index 0 of root_module_direct_dev_deps, got element of type int, want string".to_owned()),
            ("root_module_direct_deps=['a+b'], root_module_direct_dev_deps=[]", format!("in root_module_direct_deps: invalid user-provided repo name 'a+b': {repo}")),
            ("root_module_direct_deps=[''], root_module_direct_dev_deps=[]", format!("in root_module_direct_deps: invalid user-provided repo name '': {repo}")),
            ("root_module_direct_deps=[], root_module_direct_dev_deps=['_x']", format!("in root_module_direct_dev_deps: invalid user-provided repo name '_x': {repo}")),
            ("root_module_direct_deps=['a\\nb'], root_module_direct_dev_deps=[]", format!("in root_module_direct_deps: invalid user-provided repo name 'a<?>b': {repo}")),
            ("root_module_direct_deps=['a', 'b', 'a'], root_module_direct_dev_deps=[]", "in root_module_direct_deps: duplicate entry 'a'".to_owned()),
            ("root_module_direct_deps=['a'], root_module_direct_dev_deps=['b', 'a']", "in root_module_direct_dev_deps: entry 'a' is also in root_module_direct_deps".to_owned()),
            ("root_module_direct_deps=['a'], root_module_direct_dev_deps=('b', 'b')", "in root_module_direct_dev_deps: duplicate entry 'b'".to_owned()),
            ("root_module_direct_deps=['a', '-'], root_module_direct_dev_deps=['a']", format!("in root_module_direct_deps: invalid user-provided repo name '-': {repo}")),
            // ArgumentProcessor: unknown keywords fail immediately, positionals last.
            ("fact={}", "extension_metadata() got unexpected keyword argument 'fact' (did you mean 'facts'?)".to_owned()),
            ("reproducable=True", "extension_metadata() got unexpected keyword argument 'reproducable' (did you mean 'reproducible'?)".to_owned()),
            ("[], []", "extension_metadata() got unexpected positional argument".to_owned()),
            ("None, reproducible=1", "parameter 'reproducible' got value of type 'int', want 'bool'".to_owned()),
            ("None, unknown=1", "extension_metadata() got unexpected keyword argument 'unknown'".to_owned()),
        ] {
            let body = format!("return ctx.extension_metadata({args})");
            let error = fixture.run(&body, "{}", &[]).unwrap_err();
            assert!(error.contains(&expected), "{args}: expected {expected:?}: {error}");
        }
        let error = fixture
            .run("ctx.extension_metadata(zzz=1)", "{}", &[])
            .unwrap_err();
        assert!(
            error.contains("extension_metadata() got unexpected keyword argument 'zzz'")
                && !error.contains("did you mean"),
            "{error}"
        );
    }

    #[test]
    fn spell_checker_matches_starlark_java() {
        assert_eq!(
            did_you_mean("fact", &METADATA_PARAMS),
            " (did you mean 'facts'?)"
        );
        assert_eq!(
            did_you_mean("FACTS", &METADATA_PARAMS),
            " (did you mean 'facts'?)"
        );
        assert_eq!(
            did_you_mean("root_module_direct_dep", &METADATA_PARAMS),
            " (did you mean 'root_module_direct_deps'?)"
        );
        assert_eq!(
            did_you_mean("root_module_dev_deps", &METADATA_PARAMS),
            " (did you mean 'root_module_direct_deps'?)"
        );
        assert_eq!(did_you_mean("x", &METADATA_PARAMS), "");
        assert_eq!(did_you_mean("", &METADATA_PARAMS), "");
    }

    #[test]
    fn module_download_real_signature_result_read_and_tuple_urls() {
        use crate::extension_io::tests::{Server, response};
        let body = b"{\"versions\": [\"1.99.1\"]}";
        let server = Server::new(vec![response(200, body, "")]);
        let sha = crate::extension_download::sha256(body);
        let fixture = Fixture::new();
        let result = fixture.run(&format!(r#"
result = ctx.download(({url:?},), 'data', {sha:?}, False, False, '', {{}}, {{}}, integrity='', block=True)
type(result) == 'struct' or fail('not a struct')
sorted(dir(result)) == ['integrity', 'sha256', 'size_bytes', 'success'] or fail(dir(result))
result.success and result.sha256 == {sha:?} and result.size_bytes == {size} or fail(result)
json.decode(ctx.read('data', watch='auto'))['versions'][0] == '1.99.1' or fail('not actual JSON')
ctx.download([], 'data', {sha:?}, allow_fail=True).success or fail('reuse failed')
r(name='actual', words=[ctx.read('data', watch='no')])
"#, url=server.url, size=body.len()), "{}", &[]).unwrap();
        assert_eq!(server.count(), 1);
        // Reads of the working directory are never recorded (java:1669-1676).
        assert!(result.inputs.files.is_empty());
        assert!(result.inputs.host_facilities.contains("download"));
        assert_eq!(result.repositories.len(), 1);
    }

    #[test]
    fn module_download_block_false_wait_auth_headers_and_executable() {
        use crate::extension_io::tests::{Server, response};
        let body = b"#!/bin/sh\necho ok\n";
        let server = Server::new(vec![
            response(200, body, ""),
            response(404, b"", ""),
            response(200, body, ""),
        ]);
        let sha = crate::extension_download::sha256(body);
        let fixture = Fixture::new();
        let url = format!("{}/tool", server.url);
        fixture
            .run(
                &format!(
                    r#"
pending = ctx.download({url:?}, 'tool', sha256={sha:?}, executable=True, block=False,
    headers={{'X-Single': 'one', 'X-Multi': ['a', 'b']}},
    auth={{{url:?}: {{'type': 'basic', 'login': 'user', 'password': 'pw'}}}})
type(pending) == 'PendingDownload' or fail(type(pending))
repr(pending) == "<pending download to '%s'>" % ctx.path('tool') or fail(repr(pending))
dir(pending) == ['wait'] or fail(dir(pending))
r = pending.wait()
r.success and r.sha256 == {sha:?} or fail(r)
pending.wait().success or fail('wait twice')
ctx.execute(['test', '-x', ctx.path('tool')]).return_code == 0 or fail('executable')
missing = ctx.download({url:?} + '-missing', 'missing', block=False, allow_fail=True)
missing.wait().success == False or fail('allow_fail')
# The repository download cache serves a known checksum without a request.
ctx.download({url:?} + '-missing2', 'cached', sha256={sha:?}, block=False).wait().success or fail('cache')
ctx.read('cached') == ctx.read('tool') or fail('cached bytes')
"#
                ),
                "{}",
                &[],
            )
            .unwrap();
        let requests = server.requests.lock().unwrap().clone();
        assert!(requests[0].contains("x-single: one\r\n"), "{}", requests[0]);
        assert!(requests[0].contains("x-multi: a\r\n"), "{}", requests[0]);
        assert!(requests[0].contains("x-multi: b\r\n"), "{}", requests[0]);
        assert!(
            requests[0].contains("authorization: Basic dXNlcjpwdw==\r\n"),
            "{}",
            requests[0]
        );
        assert_eq!(requests.len(), 1, "{requests:?}");
        // A failed pending download raises from wait() only.
        let server = Server::new(vec![response(404, b"", "")]);
        let never = crate::extension_download::sha256(b"never served");
        fixture.error(
            &format!(
                "p = ctx.download('{}/x', 'x', sha256='{never}', block=False)\np.wait()",
                server.url
            ),
            "{}",
            &[],
            "Error downloading",
        );
        for (body, expected) in [
            (
                "ctx.download('https://localhost/x', auth={'https://localhost/x': {'type': 'basic', 'login': 'u'}})",
                "Found request to do basic auth for https://localhost/x without 'login' and 'password' being provided.",
            ),
            (
                "ctx.download('https://localhost/x', auth={'https://localhost/x': {'type': 'pattern'}})",
                "Found request to do pattern auth for https://localhost/x without a pattern being provided",
            ),
            (
                "ctx.download('https://localhost/x', auth={'https://localhost/x': {'type': 'pattern', 'pattern': 'Bearer <password>'}})",
                "Auth pattern contains <password> but it was not provided in auth dict.",
            ),
            (
                "ctx.download('https://localhost/x', headers={'X': 1})",
                "headers argument must be a dict whose keys are string and whose values are either string or sequence of string",
            ),
            (
                "ctx.download('https://localhost/x', headers={'X': [1]})",
                "at index 0 of header values, got element of type int, want string",
            ),
            (
                "ctx.download('https://localhost/x', auth={'k': 1})",
                "got dict<string, int> for 'auth', want dict<string, dict>",
            ),
        ] {
            fixture.error(body, "{}", &[], expected);
        }
    }

    #[test]
    fn module_download_over_verified_tls_reads_real_json() {
        let server = crate::extension_io::tests::TlsServer::new();
        let fixture = Fixture::new();
        let working = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let context = Context {
            repositories: fixture.repositories.clone(),
            public_label: label(),
            calls: CallRecorder::default(),
            base: crate::repository_ctx::Context::module_extension(
                fixture.repositories.clone(),
                working.path(),
                label(),
                "//:defs.bzl%ext".to_owned(),
            )
            .unwrap()
            .with_client(crate::extension_download::trusted_client(&server.pem)),
        };
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            module.set(
                "ctx",
                heap.alloc(ModuleCtxGen {
                    modules: Value::new_none(),
                    os: Value::new_none(),
                    facts: heap.alloc(values::Facts {
                        members: Vec::new(),
                    }),
                    root_has_non_dev_dependency: false,
                }),
            );
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(starlark::syntax::AstModule::parse("tls.star", format!("r = ctx.download({:?}, 'data')\nr.success and r.size_bytes == 20 or fail(r)\njson.decode(ctx.read('data'))['real'] == 'tls bytes' or fail('not real data')\n", server.url), &starlark::syntax::Dialect::Standard).unwrap(), &crate::package::bzl_globals()).unwrap();
        });
        let Context { base, .. } = context;
        assert!(base.finish().unwrap().host_facilities.contains("download"));
    }

    #[test]
    fn actual_module_context_host_reads_do_not_require_flags_or_capture_environment() {
        use crate::repository_environment::tests::Reader;
        let mut fixture = Fixture::new();
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader.clone()),
        );
        let result = fixture.run("host = ctx.os\n(host == ctx.os and {host:42}[ctx.os] == 42) or fail('identity')\n(type(host) == 'repository_os' and host.name == 'linux' and host.arch == 'amd64') or fail('metadata')\n(dir(host) == ['arch', 'environ', 'name'] and hasattr(host, 'environ')) or fail('surface')\nr(name='metadata', words=[host.name, host.arch])", "{}", &[]).unwrap();
        assert_eq!(result.repositories.len(), 1);
        assert_eq!(reader.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        fixture.error(
            "json.encode(ctx.os)",
            "{}",
            &[],
            "cannot encode unsupported value as JSON",
        );
        assert_eq!(reader.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn module_ctx_environ_getenv_and_declared_environ_are_bazel_repo_env_inputs() {
        use crate::repository_environment::tests::Reader;
        let mut fixture = Fixture::new();
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader.clone()),
        );
        // module_ctx.os.environ: a fresh mutable dict of the client environment
        // that records no dependency; module_ctx.getenv records the variable.
        let unread = fixture
            .run("e = ctx.os.environ\n(type(e) == 'dict' and getattr(ctx.os, 'environ', {}) == e) or fail('environ')\ne['X'] = 'y'\n('X' not in ctx.os.environ) or fail('fresh')\nr(name='m', words=[])", "{}", &[])
            .unwrap();
        assert!(unread.inputs.watched_env.is_none());
        assert_eq!(reader.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let read = fixture
            .run("r(name='m', words=[ctx.getenv('SYNTHETIC', 'default'), str(ctx.getenv('SYNTHETIC'))])", "{}", &[])
            .unwrap();
        assert!(read.inputs.watched_env.is_some());
        assert!(!format!("{:?}", read.inputs.watched_env).contains("SyntheticSecret"));
        assert!(
            !serde_json::to_string(&read.inputs.watched_env)
                .unwrap()
                .contains("SyntheticSecret")
        );
        assert_eq!(reader.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        for body in [
            "ctx.getenv(1)",
            "ctx.getenv('SYNTHETIC', 1)",
            "ctx.getenv(name='SYNTHETIC')",
            "ctx.getenv()",
        ] {
            let error = fixture.run(body, "{}", &[]).unwrap_err();
            assert!(error.contains("getenv"), "{error}");
            assert!(!error.contains("SyntheticSecret"), "{error}");
        }
        // module_extension(environ=[...]) observes declared names before the
        // callback even when unread (ModuleExtension environ).
        fixture.file("defs.bzl", "def _impl(ctx):\n    pass\next = module_extension(implementation=_impl, environ=['DECLARED'])\n");
        let loaded = load_definition_in(
            fixture.repositories.clone(),
            &label(),
            "ext",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        let declared = fixture.run_loaded(loaded, &[], true).unwrap();
        assert!(declared.inputs.watched_env.is_some());
    }

    #[test]
    fn frozen_module_context_retains_single_host_value_and_distinct_context_identity() {
        let owner = Module::with_temp_heap(|module| {
            let heap = module.heap();
            let metadata = RepositorySet::new(".".into()).host().unwrap();
            for name in ["first", "second"] {
                module.set(
                    name,
                    heap.alloc(ModuleCtxGen {
                        modules: Value::new_none(),
                        os: crate::host_os::alloc(heap, metadata.clone()).unwrap(),
                        facts: heap.alloc(values::Facts {
                            members: Vec::new(),
                        }),
                        root_has_non_dev_dependency: false,
                    }),
                );
            }
            module.freeze().unwrap()
        });
        let first = owner.get("first").unwrap();
        let second = owner.get("second").unwrap();
        drop(owner);
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            module.set("first", heap.access_owned_frozen_value(&first));
            module.set("second", heap.access_owned_frozen_value(&second));
            drop(first);
            drop(second);
            Evaluator::new(&module).eval_module(
                starlark::syntax::AstModule::parse("host.star", "(first.os.name == 'linux' and first.os.arch == 'amd64' and first.os == first.os and first.os != second.os and {first.os:42}[first.os] == 42) or fail('frozen host')".to_owned(), &starlark::syntax::Dialect::Standard).unwrap(),
                &crate::package::bzl_globals(),
            ).unwrap();
        });
    }

    #[test]
    fn lexical_helper_labels_do_not_forward_unknown_generated_siblings() {
        let fixture = Fixture::new();
        fixture.file("helper/BUILD", "");
        fixture.file(
            "helper/labels.bzl",
            "def make():\n    return Label(':lexical')\n",
        );
        let loaded = fixture.load_source("load('//helper:labels.bzl', 'make')\ndef _repo(ctx):\n    fail('not materialized')\nr = repository_rule(implementation=_repo)\ndef _impl(ctx):\n    r(name='one', src=make())\next = module_extension(_impl)");
        let result = fixture.run_loaded(loaded, &[], true).unwrap();
        assert_eq!(
            result.repositories[0].attributes["src"],
            RecordedValue::Label("//helper:lexical".to_owned())
        );
        // A generated sibling is not visible to the extension's own .bzl:
        // Label() yields an invalid Label (Label.parseWithPackageContext),
        // which fails only when its repository is used.
        fixture.error(
            "r(name='later')\nl = Label('@later//:x')\nstr(l).startswith(\"@@[unknown repo 'later' requested from @@\") or fail('visible')\nl.repo_name",
            "{}",
            &[],
            "'repo_name' is not allowed on invalid Label @@[unknown repo 'later' requested from @@",
        );
    }

    #[test]
    fn recorded_private_rule_retains_transitive_implementation_after_all_owners_drop() {
        let fixture = Fixture::new();
        fixture.file("helper.bzl", "def _private(value):\n    return 'alive:' + value\ndef run(value):\n    return _private(value)\n");
        let loaded = fixture.load_source("load(':helper.bzl', 'run')\ndef _repo(value):\n    return run(value)\n_private_rule = repository_rule(implementation=_repo)\ndef _impl(ctx):\n    _private_rule(name='kept')\n    for i in range(20000):\n        garbage = [str(i)] * 128\next = module_extension(_impl)");
        let result = fixture.run_loaded(loaded, &[], true).unwrap();
        let definition = result.repositories.into_iter().next().unwrap().definition;
        drop(fixture);
        Module::with_temp_heap(|module| {
            let implementation = module
                .heap()
                .access_owned_frozen_value(&definition.implementation);
            drop(definition);
            let result = Evaluator::new(&module)
                .eval_function(implementation, &[module.heap().alloc("after-drop")], &[])
                .unwrap();
            assert_eq!(result.unpack_str(), Some("alive:after-drop"));
        });
    }

    #[test]
    fn source_profile_and_module_metadata_are_rejected_before_binding() {
        let fixture = Fixture::new();
        let mut source = participant(vec![tag("unknown", 0, json!({}), false)]);
        source.source_profile_error = Some("hidden Unicode computation is unsupported".to_owned());
        fixture.error("fail('callback ran')", "{}", &[source], "hidden Unicode");
        let mut source = participant(Vec::new());
        source.version = "é".to_owned();
        fixture.error(
            "fail('callback ran')",
            "{}",
            &[source],
            "non-ASCII module metadata",
        );
    }

    #[test]
    fn callbacks_have_no_step_or_heap_limit() {
        // Bazel bounds neither (former native caps: 1 M ticks, 128 MiB heap).
        let fixture = Fixture::new();
        fixture
            .run(
                "for i in range(2000000):\n    x = i + 1\nbig = ['x' * 1024 for i in range(200000)]\nlen(big) == 200000 or fail('heap')",
                "{}",
                &[],
            )
            .unwrap();
    }

    #[test]
    fn label_same_package_label_relative_and_repo_root() {
        let fixture = Fixture::new();
        fixture
            .run(
                r#"
l = Label('//pkg:in.txt')
str(l.same_package_label('other')) == '@@//pkg:other' or fail(l.same_package_label('other'))
str(l.relative(':rel')) == '@@//pkg:rel' or fail('relative')
str(l.relative('//x:y')) == '@@//x:y' or fail('absolute')
l.repo_root == '' and Label('@@foo+//p:n').repo_root == 'external/foo+' or fail('repo_root')
str(Label('@@foo+//p:n').relative(':m')) == '@@foo+//p:m' or fail('other repo')
# Label.create strips a trailing '/.' (N38) like Label() does.
str(l.same_package_label('d/.')) == '@@//pkg:d' or fail(l.same_package_label('d/.'))
# An apparent repository the mapping does not see is a deferred invalid
# label (N45), like Label().
str(l.relative('@nope//:t')) == "@@[unknown repo 'nope' requested from @@]//:t" or fail(l.relative('@nope//:t'))
'relative' in dir(l) and 'same_package_label' in dir(l) and 'repo_root' in dir(l) or fail(dir(l))
"#,
                "{}",
                &[],
            )
            .unwrap();
        fixture.error(
            "Label('//pkg:x').same_package_label('a:b')",
            "{}",
            &[],
            "invalid target name 'a:b': target names may not contain ':'",
        );
    }

    #[test]
    fn tag_sort_key_orders_tags_across_classes_and_modules() {
        let fixture = Fixture::new();
        let tags = vec![
            tag("b", 0, json!({"name": "first"}), false),
            tag("a", 1, json!({"name": "second"}), false),
            tag("b", 2, json!({"name": "third"}), true),
        ];
        let other = vec![tag("a", 0, json!({"name": "fourth"}), false)];
        let mut second = participant(other);
        second.owner = "other+".to_owned();
        second.name = "other".to_owned();
        second.is_root = false;
        fixture
            .run(
                r#"
all = []
for m in ctx.modules:
    for cls in [m.tags.a, m.tags.b]:
        all.extend(cls)
ordered = sorted(all, key = lambda t: ctx.tag_sort_key(t))
[t.name for t in ordered] == ['first', 'second', 'third', 'fourth'] or fail([t.name for t in ordered])
k = ctx.tag_sort_key(ordered[1])
type(k) == 'sort_key' and repr(k) == '<sort_key>' and str(k) == '<sort_key>' or fail(repr(k))
k == ctx.tag_sort_key(ordered[1]) and {k: 1}[ctx.tag_sort_key(ordered[1])] == 1 or fail('equality/hash')
ctx.tag_sort_key(ordered[0]) < k and k < ctx.tag_sort_key(ordered[3]) or fail('order')
ctx.is_dev_dependency(ordered[2]) or fail('dev')
"#,
                "{'a': tag_class(attrs={'name': attr.string()}), 'b': tag_class(attrs={'name': attr.string()})}",
                &[participant(tags), second],
            )
            .unwrap();
        fixture.error(
            "ctx.tag_sort_key(1)",
            "{}",
            &[],
            "in call to tag_sort_key(), parameter 'tag' got value of type 'int', want 'bazel_module_tag'",
        );
    }

    #[test]
    fn private_extension_exports_execute_and_phase_fallback_stays_isolated() {
        let fixture = Fixture::new();
        fixture.file(
            "defs.bzl",
            "def _impl(ctx):\n    return None\n_private = module_extension(_impl)\n",
        );
        let loaded = crate::loading::load_module_extension_in(
            fixture.repositories.clone(),
            &label(),
            "_private",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        fixture.run_loaded(loaded, &[], true).unwrap();
        Module::with_temp_heap(|module| {
            let context = crate::loading::ExtensionContext {
                byte_strings: false,
                repositories: fixture.repositories.clone(),
                label: label(),
            };
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            assert!(repository_context(&eval).is_none());
            assert!(definition_package(&eval).is_none());
        });
    }
}
