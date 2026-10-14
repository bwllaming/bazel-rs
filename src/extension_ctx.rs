//! Bounded ordinary extension callbacks with an immutable, separately frozen
//! MODULE projection. Repository calls only record declarations: publication,
//! sibling mapping, schema binding and materialization belong to the caller.

// The generated native wrapper preserves Bazel's ten-parameter download API.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{anyhow, bail, ensure};
use serde::Serialize;
use starlark::environment::{Methods, MethodsBuilder, Module};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::structs::AllocStruct;
use starlark::values::{FrozenHeapName, Heap, OwnedFrozenValue, Value, ValueLike};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::custom_rule::AttrValue;
use crate::extension_calls::{CallRecorder, RecordedRepository};
use crate::label::{Label, PackageId};
use crate::module_extension::{ExtensionDefinition, bind_tag};
use crate::repository::RepositorySet;
use crate::repository_budget::HostBudget;
use crate::repository_catalog::OrdinaryParticipant;
use crate::starlark_types::alloc_label;

#[derive(Debug)]
pub struct ExtensionExecution {
    pub repositories: Vec<RecordedRepository>,
    pub modules: Vec<BoundModule>,
    pub(crate) io_observations: Vec<crate::extension_io::Observation>,
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
        Trace, starlark_value,
    };

    #[derive(ProvidesStaticType)]
    pub(super) struct Context {
        pub repositories: Arc<RepositorySet>,
        pub public_label: Label,
        pub calls: CallRecorder,
        pub io: RefCell<crate::extension_io::ScratchExecutionIo>,
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
    }

    impl<V> fmt::Display for TagGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<tag>")
        }
    }

    starlark_complex_value!(pub(super) Tag);

    #[starlark_value(type = "tag")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TagGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            // attrs is always an immutable struct containing only schema fields.
            // The hidden dev flag never shadows a declared dev_dependency attr.
            self.attrs.to_value().get_attr(name, heap).ok().flatten()
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
            write!(f, "<bazel_module {}>", self.name)
        }
    }

    starlark_complex_value!(pub(super) BazelModule);

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
        pub root_has_non_dev_dependency: bool,
        // Inert, never reused; no path, client, file handle or authority survives
        // freezing. I/O additionally requires this live receiver's active Context.
        pub execution_identity: u64,
    }

    impl<V> fmt::Display for ModuleCtxGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<module_ctx>")
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
                // Unsupported context facilities are absent, never dummy values.
                _ => None,
            }
        }

        fn dir_attr(&self) -> Vec<String> {
            [
                "modules",
                "os",
                "root_module_has_non_dev_dependency",
                "download",
                "read",
            ]
            .map(str::to_owned)
            .to_vec()
        }
    }
}
use values::{BazelModuleGen, Context, FrozenTag, ModuleCtxGen, Tag, TagGen};

fn io_receiver(this: Value<'_>, eval: &Evaluator<'_, '_, '_>) -> anyhow::Result<u64> {
    use crate::extension_download::TypedIoError;
    let receiver = this
        .downcast_ref::<values::ModuleCtx>()
        .ok_or(TypedIoError::Lifetime)?;
    let context = context(eval).ok_or(TypedIoError::Lifetime)?;
    context
        .io
        .borrow()
        .check_receiver(receiver.execution_identity)?;
    Ok(receiver.execution_identity)
}

fn context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> Option<&'a Context> {
    eval.extra.and_then(|extra| extra.downcast_ref::<Context>())
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

starlark::methods_static!(MODULE_METHODS = module_methods);

#[starlark_module]
fn module_methods(builder: &mut MethodsBuilder) {
    fn download<'v>(
        this: Value<'v>,
        url: Value<'v>,
        #[starlark(default = "")] output: &str,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = false)] executable: bool,
        #[starlark(default = false)] allow_fail: bool,
        #[starlark(default = "")] canonical_id: &str,
        auth: Option<Value<'v>>,
        headers: Option<Value<'v>>,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        use crate::extension_download::TypedIoError;
        let identity = io_receiver(this, eval)?;
        // All unsupported controls and all sequence elements are checked before
        // scratch creation, destination reuse, policy gating or transport.
        if executable || !block || !canonical_id.is_empty() {
            return Err(TypedIoError::Unsupported(
                "executable, async block=False, or canonical_id cache",
            )
            .into());
        }
        for option in [auth, headers].into_iter().flatten() {
            let dict = starlark::values::dict::DictRef::from_value(option).ok_or(
                TypedIoError::Validation("auth and headers must be dictionaries"),
            )?;
            if !dict.is_empty() {
                return Err(TypedIoError::Unsupported("nonempty auth or headers").into());
            }
        }
        let strings = if let Some(text) = url.unpack_str() {
            vec![text]
        } else {
            let values = if let Some(list) = starlark::values::list::ListRef::from_value(url) {
                list.content()
            } else if let Some(tuple) = starlark::values::tuple::TupleRef::from_value(url) {
                tuple.content()
            } else {
                return Err(TypedIoError::Validation(
                    "url must be a string, list, or tuple of strings",
                )
                .into());
            };
            if values.len() > 8 {
                return Err(TypedIoError::Resource("8 mirrors per call").into());
            }
            values
                .iter()
                .map(|v| {
                    v.unpack_str()
                        .ok_or(TypedIoError::Validation("every URL must be a string"))
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let context = context(eval).ok_or(TypedIoError::Lifetime)?;
        let mut io = context.io.borrow_mut();
        let result = io.download(
            identity,
            context.repositories.repository_downloads_enabled(),
            crate::extension_io::Download {
                urls: &strings,
                output,
                sha256,
                integrity,
                allow_fail,
            },
        )?;
        let heap = eval.heap();
        let mut fields = vec![("success", heap.alloc(result.success))];
        if let Some(integrity) = &result.integrity {
            fields.push(("integrity", heap.alloc(integrity.as_str())));
        }
        if let Some(sha256) = &result.sha256 {
            fields.push(("sha256", heap.alloc(sha256.as_str())));
        }
        if let Some(size) = result.size_bytes {
            fields.push(("size_bytes", heap.alloc(size as i64)));
        }
        Ok(heap.alloc(AllocStruct(fields)))
    }

    fn read<'v>(
        this: Value<'v>,
        path: &str,
        #[starlark(require = named, default = "auto")] watch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        let identity = io_receiver(this, eval)?;
        Ok(context(eval)
            .ok_or(crate::extension_download::TypedIoError::Lifetime)?
            .io
            .borrow_mut()
            .read(identity, path, watch)?)
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
}

fn bind_modules(
    repositories: &RepositorySet,
    definition: &ExtensionDefinition,
    participants: &[OrdinaryParticipant],
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
    let mut budget = HostBudget::default();
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
    let resolution_budget = RefCell::new(HostBudget::default());
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
                let attributes = bind_tag(schema, &tag.attributes, |text| {
                    let mapping = repositories.mapping(&participant.owner)?;
                    resolution_budget
                        .borrow_mut()
                        .bytes(Label::resolution_size_bound(
                            text,
                            &participant.owner,
                            "",
                            mapping,
                        ))?;
                    repositories.parse(text, &participant.owner, "")
                })
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
        for bound in modules {
            let mut classes = BTreeMap::new();
            for (name, tags) in &bound.tags {
                let mut views = Vec::new();
                for tag in tags {
                    views.push(heap.alloc(TagGen {
                        attrs: alloc_attrs(heap, &tag.attributes)?,
                        dev_dependency: tag.dev_dependency,
                    }));
                }
                classes.insert(name.clone(), heap.alloc(views));
            }
            views.push(heap.alloc(BazelModuleGen {
                name: bound.name.clone(),
                version: bound.version.clone(),
                is_root: bound.is_root,
                tags: heap.alloc(AllocStruct(classes)),
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
pub fn execute(
    repositories: Arc<RepositorySet>,
    definition: &ExtensionDefinition,
    functions: &[OwnedFrozenValue],
    public_label: &Label,
    participants: &[OrdinaryParticipant],
    root_has_non_dev_dependency: bool,
) -> Result<ExtensionExecution, String> {
    let run = || -> anyhow::Result<ExtensionExecution> {
        let modules =
            bind_modules(&repositories, definition, participants).map_err(anyhow::Error::msg)?;
        let projected = projection(&modules)?;
        let context = Context {
            repositories,
            public_label: public_label.clone(),
            calls: CallRecorder::default(),
            io: RefCell::new(crate::extension_io::ScratchExecutionIo::new()?),
        };
        let repositories = Module::with_temp_heap(|module| -> anyhow::Result<_> {
            let heap = module.heap();
            let ctx = heap.alloc(ModuleCtxGen {
                modules: heap.access_owned_frozen_value(&projected),
                os: crate::host_os::alloc(
                    heap,
                    context.repositories.host().map_err(anyhow::Error::msg)?,
                )?,
                root_has_non_dev_dependency,
                execution_identity: context.io.borrow().identity(),
            });
            for function in functions {
                heap.access_owned_frozen_value(function);
            }
            let implementation = heap.access_owned_frozen_value(&definition.implementation);
            let mut eval = Evaluator::new(&module);
            eval.set_max_callstack_size(128)?;
            eval.set_max_tick_count(1_000_000)?;
            eval.set_max_heap_size(128 * 1024 * 1024)?;
            eval.extra = Some(&context);
            let evaluated = eval.eval_function(implementation, &[ctx], &[]);
            // No live HTTP/file handle remains in the synchronous authority.
            // Close even when evaluation failed, BEFORE success or heap freeze.
            let observations = context.io.borrow_mut().close();
            let returned = evaluated.map_err(|error| anyhow!(error.to_string()))?;
            let observations = observations?;
            ensure!(
                returned.is_none(),
                "module extension implementation return values other than None are unsupported"
            );
            drop(eval);
            let frozen = module
                .freeze_named(FrozenHeapName::User(Box::new(format!(
                    "ordinary extension callback:{public_label}"
                ))))
                .map_err(|error| anyhow!("cannot freeze module extension callback: {error:?}"))?;
            Ok((
                context.calls.finish(&frozen).map_err(anyhow::Error::msg)?,
                observations,
            ))
        })?;
        Ok(ExtensionExecution {
            repositories: repositories.0,
            modules,
            io_observations: repositories.1,
        })
    };
    run().map_err(|error| format!("module extension {public_label}: {error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("../tests/support/extension_io_callback_budget.rs");
    include!("../tests/support/extension_io_preflight_cases.rs");
    include!("../tests/support/extension_io_boundary_context.rs");
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
            let root = tempfile::tempdir_in("/tmp/opencode").unwrap();
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
                participants,
                non_dev,
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
            declaration: 0,
            dev_dependency,
        }
    }

    #[test]
    fn repeated_tag_defaults_fail_before_binding_or_projection() {
        let fixture = Fixture::new();
        let classes = "{'t':tag_class({'value':attr.string(default='x' * 1048576)})}";
        let one = [participant(vec![tag("t", 0, json!({}), false)])];
        fixture.error(
            "fail('CALLBACK_REACHED')",
            classes,
            &one,
            "CALLBACK_REACHED",
        );
        let many = [participant(
            (0..600).map(|i| tag("t", i, json!({}), false)).collect(),
        )];
        let error = fixture
            .run("fail('CALLBACK_REACHED')", classes, &many)
            .unwrap_err();
        assert!(
            error.contains("host preparation weighted byte limit"),
            "{error}"
        );
        assert!(!error.contains("CALLBACK_REACHED"), "{error}");
        assert!(!fixture.root.path().join("cache").exists());
    }

    #[test]
    fn repeated_repository_defaults_fail_during_recording_not_finish() {
        let fixture = Fixture::new();
        for count in [1, 600] {
            let loaded = fixture.load_source(&format!(
                "def _repo(ctx):\n    fail('FETCH_REACHED')\nr = repository_rule(_repo, attrs={{'value':attr.string(default='x' * 1048576)}})\ndef _impl(ctx):\n    for i in range({count}):\n        r(name='made' + str(i))\n    fail('CALLBACK_COMPLETED')\next = module_extension(_impl)\n"
            ));
            let error = fixture.run_loaded(loaded, &[], true).unwrap_err();
            if count == 1 {
                assert!(error.contains("CALLBACK_COMPLETED"), "{error}");
            } else {
                assert!(
                    error.contains("host preparation weighted byte limit"),
                    "{error}"
                );
                assert!(!error.contains("fail: CALLBACK_COMPLETED"), "{error}");
            }
        }
        // Cost estimation must not eagerly bind unknown/missing mandatory attrs.
        let loaded = fixture.load_source("def _repo(ctx):\n    pass\nr = repository_rule(_repo, attrs={'required':attr.string(mandatory=True)})\ndef _impl(ctx):\n    r(name='made', unknown=True)\n    fail('DEFERRED_BINDING')\next = module_extension(_impl)\n");
        let error = fixture.run_loaded(loaded, &[], true).unwrap_err();
        assert!(error.contains("DEFERRED_BINDING"), "{error}");
        assert!(!fixture.root.path().join("cache").exists());
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
(type(ctx) == 'module_ctx' and type(m) == 'bazel_module' and type(t) == 'tag') or fail('types')
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
                "Immutable",
            );
        }
        for assignment in [
            "ctx.modules = []",
            "ctx.modules[0].name = 'changed'",
            "ctx.modules[0].tags.t = []",
            "ctx.modules[0].tags.t[0].words = []",
        ] {
            fixture.error(assignment, classes, &participants, "not supported");
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
        fixture.error(
            "ctx.root_module_has_non_dev_dependency()",
            classes,
            &participants,
            "not supported",
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
                    root_has_non_dev_dependency: false,
                    execution_identity: 0,
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
        assert!(!fixture.root.path().join("cache").exists());
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
        for returned in ["42", "{}", "struct(root_module_direct_deps=[])"] {
            fixture.error(
                &format!("r(name='first')\nreturn {returned}"),
                "{}",
                &[],
                "other than None",
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
        assert!(!fixture.root.path().join("cache").exists());
    }

    #[test]
    fn unsupported_context_facilities_are_absent_not_fabricated() {
        let fixture = Fixture::new();
        for field in [
            "extension_metadata",
            "facts",
            "path",
            "delete",
            "tag_sort_key",
            "is_isolated",
            "file",
            "execute",
        ] {
            fixture.error(&format!("ctx.{field}"), "{}", &[], field);
        }
        fixture.error(
            "ctx.os.environ",
            "{}",
            &[],
            "repository_os.environ is unsupported",
        );
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

    #[test]
    fn module_download_real_signature_result_read_and_tuple_urls() {
        use crate::extension_io::tests::{Server, response};
        let body = b"{\"versions\": [\"1.99.1\"]}";
        let server = Server::new(vec![response(200, body, "")]);
        let sha = crate::extension_download::sha256(body);
        let fixture = Fixture::new();
        let result = fixture.run(&format!(r#"
hasattr(ctx, 'download') or fail('download absent')
hasattr(ctx, 'read') or fail('read absent')
'download' in dir(ctx) or fail('dir download absent')
'read' in dir(ctx) or fail('dir read absent')
result = ctx.download(({url:?},), 'data', {sha:?}, False, False, '', {{}}, {{}}, integrity='', block=True)
type(result) == 'struct' or fail('not a struct')
sorted(dir(result)) == ['integrity', 'sha256', 'size_bytes', 'success'] or fail(dir(result))
result.success and result.sha256 == {sha:?} and result.size_bytes == {size} or fail(result)
json.decode(ctx.read(path='data', watch='auto'))['versions'][0] == '1.99.1' or fail('not actual JSON')
ctx.download([], 'data', {sha:?}, allow_fail=True).success or fail('reuse failed')
r(name='actual', words=[ctx.read('data', watch='no')])
"#, url=server.url, size=body.len()), "{}", &[]).unwrap();
        assert_eq!(server.count(), 1);
        assert_eq!(result.io_observations.len(), 4);
        assert_eq!(result.repositories.len(), 1);
    }

    #[test]
    fn module_download_preflight_types_and_unsupported_controls_make_no_requests() {
        use crate::extension_io::tests::{Server, response};
        let server = Server::new(vec![response(200, b"unused", "")]);
        let fixture = Fixture::new();
        let base = format!(
            "ctx.download({:?}, sha256={:?}",
            server.url,
            "00".repeat(32)
        );
        for suffix in [
            ", executable=True)",
            ", block=False)",
            ", canonical_id='cache')",
            ", auth={'x': {}})",
            ", headers={'x': 'y'})",
            ", auth=None)",
            ", headers=[])",
            ", executable=1)",
            ", allow_fail=1)",
            ", block='yes')",
            ", output=Label('//:file'))",
            ", integrity='sha256-AAAA')",
        ] {
            assert!(
                fixture.run(&(base.clone() + suffix), "{}", &[]).is_err(),
                "{suffix}"
            );
        }
        for body in [
            "ctx.download([\"https://localhost\", 1], allow_fail=True)",
            "ctx.download(1, allow_fail=True)",
            "ctx.download({'https://localhost': 1}, allow_fail=True)",
            "ctx.download([], '', '', False, True, '', {}, {}, 'sha256-AAAA')",
            "ctx.download([], unknown=True)",
            "ctx.read('file', 'no')",
            "ctx.read('file', watch=1)",
            "ctx.read('file', watch='invalid')",
            "ctx.read(Label('//:file'))",
        ] {
            assert!(fixture.run(body, "{}", &[]).is_err(), "{body}");
        }
        assert_eq!(server.count(), 0);
        fixture.run("result = ctx.download([], allow_fail=True)\ndir(result) == ['success'] and result.success == False or fail(result)", "{}", &[]).unwrap();
    }

    #[test]
    fn foreign_frozen_and_expired_module_receivers_cannot_borrow_authority() {
        let fixture = Fixture::new();
        let io = crate::extension_io::ScratchExecutionIo::new().unwrap();
        let id = io.identity();
        let context = Context {
            repositories: fixture.repositories.clone(),
            public_label: label(),
            calls: CallRecorder::default(),
            io: RefCell::new(io),
        };
        let frozen = Module::with_temp_heap(|module| {
            module.set(
                "ctx",
                module.heap().alloc(ModuleCtxGen {
                    modules: Value::new_none(),
                    os: Value::new_none(),
                    root_has_non_dev_dependency: false,
                    execution_identity: id,
                }),
            );
            module.freeze().unwrap().get("ctx").unwrap()
        });
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let foreign = heap.alloc(ModuleCtxGen {
                modules: Value::new_none(),
                os: Value::new_none(),
                root_has_non_dev_dependency: false,
                execution_identity: id + 1,
            });
            let own = heap.alloc(ModuleCtxGen {
                modules: Value::new_none(),
                os: Value::new_none(),
                root_has_non_dev_dependency: false,
                execution_identity: id,
            });
            let mut eval = Evaluator::new(&module);
            assert!(io_receiver(own, &eval).is_err());
            eval.extra = Some(&context);
            assert!(io_receiver(foreign, &eval).is_err());
            assert!(io_receiver(heap.access_owned_frozen_value(&frozen), &eval).is_err());
            assert_eq!(io_receiver(own, &eval).unwrap(), id);
            context.io.borrow_mut().close().unwrap();
            assert!(io_receiver(own, &eval).is_err());
        });
    }

    #[test]
    fn actual_context_checksumless_private_tls_reads_real_json_and_cleans_before_freeze() {
        let server = crate::extension_io::tests::TlsServer::new();
        let fixture = Fixture::new();
        let io = crate::extension_io::ScratchExecutionIo::new()
            .unwrap()
            .with_client(crate::extension_download::trusted_client(&server.pem));
        let identity = io.identity();
        let context = Context {
            repositories: fixture.repositories.clone(),
            public_label: label(),
            calls: CallRecorder::default(),
            io: RefCell::new(io),
        };
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            module.set(
                "ctx",
                heap.alloc(ModuleCtxGen {
                    modules: Value::new_none(),
                    os: Value::new_none(),
                    root_has_non_dev_dependency: false,
                    execution_identity: identity,
                }),
            );
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(starlark::syntax::AstModule::parse("tls.star", format!("r = ctx.download({:?}, 'data')\nr.success and r.size_bytes == 20 or fail(r)\njson.decode(ctx.read('data'))['real'] == 'tls bytes' or fail('not real data')\n", server.url), &starlark::syntax::Dialect::Standard).unwrap(), &crate::package::bzl_globals()).unwrap();
            drop(eval);
            let observations = context.io.borrow_mut().close().unwrap();
            assert_eq!(observations.len(), 2);
            assert!(context.io.borrow().check_receiver(identity).is_err());
            module.freeze().unwrap();
        });
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
            "getattr(ctx.os, 'environ', {})",
            "{}",
            &[],
            "repository_os.environ is unsupported",
        );
        fixture.error(
            "json.encode(ctx.os)",
            "{}",
            &[],
            "cannot encode unsupported value as JSON",
        );
        fixture.error("ctx.getenv('SYNTHETIC')", "{}", &[], "getenv");
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
                        root_has_non_dev_dependency: false,
                        execution_identity: 0,
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
        fixture.error("r(name='later')\nLabel('@later//:x')", "{}", &[], "later");
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
    fn callback_tick_limit_is_enforced() {
        let fixture = Fixture::new();
        fixture.error("for i in range(2000000):\n    x = i + 1", "{}", &[], "tick");
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
