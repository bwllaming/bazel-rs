//! User-defined providers with stable extension-label/export-name identity.
//!
//! Provider fields may contain live Starlark values during analysis. The native
//! analysis caller must freeze returned instances before handing them to another
//! target; the derived Trace/Freeze implementation traverses every field.

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::collections::StarlarkHasher;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;
use starlark::values::{
    Coerce, Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkPagablePanic,
    StarlarkValue, Trace, Value, ValueLike, starlark_value,
};
use starlark::{starlark_complex_value, starlark_module, starlark_simple_value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::hash::Hash;
use std::sync::OnceLock;

use crate::loading::ExtensionContext;
use crate::starlark_types::{File, is_default_info, unpack_label};

// Only derive-generated marker/trace/coercion implementations need this lint
// exception. Owned constructor metadata contains no Starlark heap references.
#[allow(unsafe_code)]
mod values {
    use super::*;

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct ProviderConstructor {
        pub extension: String,
        // Written once by export_as, never changed by later aliases/re-exports.
        // OnceLock is Sync, so freezing this owned value is safe and preserves
        // the exact identity without a mutable Starlark-heap reference.
        pub exported: OnceLock<String>,
        pub fields: Option<BTreeSet<String>>,
        pub _doc: Option<String>,
    }

    #[derive(
        Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, Freeze, Coerce, StarlarkPagable,
    )]
    #[repr(C)]
    pub(super) struct ProviderInstanceGen<V> {
        pub identity: String,
        pub name: String,
        // Sorted by field name for deterministic repr, comparison and hashing.
        pub fields: Vec<(String, V)>,
    }
}
use values::{ProviderConstructor, ProviderInstanceGen};

starlark_simple_value!(ProviderConstructor);
starlark_complex_value!(ProviderInstance);

impl ProviderConstructor {
    fn identity(&self) -> Option<String> {
        self.exported
            .get()
            .map(|name| format!("{}%{name}", self.extension))
    }
}

impl fmt::Display for ProviderConstructor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.identity() {
            Some(identity) => write!(f, "<provider {identity}>"),
            None => write!(f, "<unexported provider from {}>", self.extension),
        }
    }
}

#[starlark_value(type = "Provider")]
impl<'v> StarlarkValue<'v> for ProviderConstructor {
    fn export_as(&self, name: &str, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
        if self.exported.get().is_none() {
            let context = eval
                .extra
                .and_then(|extra| extra.downcast_ref::<ExtensionContext>());
            if context.is_none_or(|context| context.label.to_string() != self.extension) {
                return Err(starlark::Error::new_other(anyhow!(
                    "provider must be exported in its defining extension {}",
                    self.extension
                )));
            }
            let _ = self.exported.set(name.to_owned());
        }
        Ok(())
    }

    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let identity = self.identity().ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "provider must be assigned to a global exported name before constructing instances"
            ))
        })?;
        args.no_positional_args(eval.heap())?;
        let mut fields = Vec::new();
        for (name, value) in args.names_map()?.iter() {
            let name = name.as_str();
            if self
                .fields
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(name))
            {
                return Err(starlark::Error::new_other(anyhow!(
                    "provider {identity}: unknown field {name:?}"
                )));
            }
            fields.push((name.to_owned(), *value));
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(eval.heap().alloc(ProviderInstanceGen {
            identity,
            name: self
                .exported
                .get()
                .expect("export identity checked")
                .clone(),
            fields,
        }))
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(self
            .identity()
            .is_some_and(|identity| provider_identity(other).as_ref() == Some(&identity)))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.identity()
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow!("unexported provider cannot be hashed"))
            })?
            .hash(hasher);
        Ok(())
    }
}

impl<V> fmt::Display for ProviderInstanceGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(...)", self.name)
    }
}

// Bazel custom-provider instances are structs; the native downcast, rather than
// the display type string, distinguishes them from ordinary struct() values.
#[starlark_value(type = "struct")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ProviderInstanceGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, name: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.fields
            .binary_search_by(|(key, _)| key.as_str().cmp(name))
            .ok()
            .map(|index| self.fields[index].1.to_value())
    }

    fn dir_attr(&self) -> Vec<String> {
        self.fields.iter().map(|(name, _)| name.clone()).collect()
    }

    fn collect_repr(&self, collector: &mut String) {
        collector.push_str(&self.name);
        collector.push('(');
        for (index, (name, value)) in self.fields.iter().enumerate() {
            if index != 0 {
                collector.push_str(", ");
            }
            collector.push_str(name);
            collector.push_str(" = ");
            value.to_value().collect_repr(collector);
        }
        collector.push(')');
    }

    fn collect_repr_cycle(&self, collector: &mut String) {
        collector.push_str(&self.name);
        collector.push_str("(...)");
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        let Some(other) = ProviderInstance::from_value(other) else {
            return Ok(false);
        };
        if self.identity != other.identity || self.fields.len() != other.fields.len() {
            return Ok(false);
        }
        for ((name, value), (other_name, other_value)) in self.fields.iter().zip(&other.fields) {
            if name != other_name || !value.to_value().equals(*other_value)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.identity.hash(hasher);
        for (name, value) in &self.fields {
            name.hash(hasher);
            value.to_value().write_hash(hasher)?;
        }
        Ok(())
    }
}

/// Stable identity of an exported constructor, never of an instance.
pub fn provider_identity(value: Value<'_>) -> Option<String> {
    if crate::template_variable_info::is_constructor(value) {
        return Some(crate::template_variable_info::PROVIDER_ID.to_owned());
    }
    ProviderConstructor::from_value(value)?.identity()
}

/// Stable identity of an instance, never of a constructor or ordinary struct.
pub fn instance_identity(value: Value<'_>) -> Option<String> {
    if crate::template_variable_info::is_instance(value) {
        return Some(crate::template_variable_info::PROVIDER_ID.to_owned());
    }
    Some(ProviderInstance::from_value(value)?.identity.clone())
}

fn sequence(value: Value<'_>) -> anyhow::Result<Vec<Value<'_>>> {
    if let Some(list) = ListRef::from_value(value) {
        Ok(list.iter().collect())
    } else if let Some(tuple) = TupleRef::from_value(value) {
        Ok(tuple.iter().collect())
    } else {
        bail!("expected list or tuple, got {}", value.get_type())
    }
}

/// Validate an implementation return value and collect custom/native providers. The
/// returned Values still belong to the analysis heap and must be frozen by the
/// caller before crossing target boundaries. DefaultInfo is handled separately.
pub fn returned_custom_providers<'v>(
    result: Value<'v>,
) -> anyhow::Result<BTreeMap<String, Value<'v>>> {
    let mut providers = BTreeMap::new();
    if result.is_none() {
        return Ok(providers);
    }
    let values = if is_default_info(result) || instance_identity(result).is_some() {
        vec![result]
    } else {
        sequence(result)?
    };
    for value in values {
        if is_default_info(value) {
            continue;
        }
        let identity = instance_identity(value).ok_or_else(|| {
            anyhow!(
                "unsupported returned provider {}; expected a provider instance",
                value.get_type()
            )
        })?;
        if providers.insert(identity.clone(), value).is_some() {
            bail!("implementation returned provider {identity} more than once");
        }
    }
    Ok(providers)
}

/// Collect files reachable through supported provider payloads, preserving first
/// encounter order. Unknown value kinds and cyclic/overdeep containers fail
/// explicitly rather than silently losing dependencies. String depsets and
/// primitive metadata (including Labels) contain no files.
pub fn provider_files(value: Value<'_>) -> anyhow::Result<Vec<File>> {
    const MAX_DEPTH: usize = 128;
    struct Walk<'v> {
        active: HashSet<starlark::values::ValueIdentity<'v>>,
        visited: HashSet<starlark::values::ValueIdentity<'v>>,
        seen_files: BTreeSet<File>,
        files: Vec<File>,
    }
    impl<'v> Walk<'v> {
        fn file(&mut self, file: File) {
            if self.seen_files.insert(file.clone()) {
                self.files.push(file);
            }
        }

        fn walk(&mut self, value: Value<'v>, depth: usize) -> anyhow::Result<()> {
            if depth > MAX_DEPTH {
                bail!("provider payload exceeds nesting depth limit {MAX_DEPTH}");
            }
            let identity = value.identity();
            if self.active.contains(&identity) {
                bail!("cycle in provider payload");
            }
            if !self.visited.insert(identity) {
                return Ok(());
            }
            self.active.insert(identity);
            if let Some(file) = File::from_value(value) {
                self.file(file.clone());
            } else if crate::template_variable_info::is_instance(value) {
                // Its fully owned string-to-string snapshot cannot contain files.
            } else if let Some(files) = crate::starlark_types::contained_depset_files(value) {
                for file in files {
                    self.file(file);
                }
            } else if let Some(provider) = ProviderInstance::from_value(value) {
                for (_, field) in &provider.fields {
                    self.walk(*field, depth + 1)?;
                }
            } else if let Some(list) = ListRef::from_value(value) {
                for child in list.iter() {
                    self.walk(child, depth + 1)?;
                }
            } else if let Some(tuple) = TupleRef::from_value(value) {
                for child in tuple.iter() {
                    self.walk(child, depth + 1)?;
                }
            } else if let Some(dict) = DictRef::from_value(value) {
                for (key, child) in dict.iter() {
                    self.walk(key, depth + 1)?;
                    self.walk(child, depth + 1)?;
                }
            } else if let Some(structure) = StructRef::from_value(value) {
                for (_, child) in structure.iter() {
                    self.walk(child, depth + 1)?;
                }
            } else if !matches!(
                value.get_type(),
                "NoneType" | "bool" | "int" | "float" | "string"
            ) && unpack_label(value).is_none()
            {
                bail!("unsupported provider payload type {}", value.get_type());
            }
            self.active.remove(&identity);
            Ok(())
        }
    }
    let mut walk = Walk {
        active: HashSet::new(),
        visited: HashSet::new(),
        seen_files: BTreeSet::new(),
        files: Vec::new(),
    };
    walk.walk(value, 0)?;
    Ok(walk.files)
}

fn field_schema(value: Value<'_>) -> anyhow::Result<Option<BTreeSet<String>>> {
    if value.is_none() {
        return Ok(None);
    }
    let mut fields = BTreeSet::new();
    let names = if let Some(dict) = DictRef::from_value(value) {
        let mut names = Vec::new();
        for (key, doc) in dict.iter() {
            if doc.unpack_str().is_none() {
                bail!("provider field documentation must be a string");
            }
            names.push(key);
        }
        names
    } else {
        sequence(value).map_err(|_| {
            anyhow!(
                "provider fields must be a list/tuple of strings or a string-to-documentation dict"
            )
        })?
    };
    for name in names {
        let name = name
            .unpack_str()
            .ok_or_else(|| anyhow!("provider field names must be strings"))?;
        if !fields.insert(name.to_owned()) {
            bail!("duplicate provider field {name:?}");
        }
    }
    Ok(Some(fields))
}

#[starlark_module]
pub fn globals(builder: &mut GlobalsBuilder) {
    fn provider<'v>(
        #[starlark(default = NoneType)] doc: Value<'v>,
        #[starlark(require = named, default = NoneType)] fields: Value<'v>,
        #[starlark(require = named, default = NoneType)] init: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let context = eval.extra.and_then(|extra| extra.downcast_ref::<ExtensionContext>())
            .ok_or_else(|| anyhow!("provider() may only be defined while loading a .bzl extension; BUILD macros and analysis are unsupported"))?;
        if !init.is_none() {
            bail!("provider(init=...) is unsupported");
        }
        let doc = if doc.is_none() {
            None
        } else {
            Some(
                doc.unpack_str()
                    .ok_or_else(|| anyhow!("provider doc must be a string or None"))?
                    .to_owned(),
            )
        };
        Ok(eval.heap().alloc(ProviderConstructor {
            extension: context.label.to_string(),
            exported: OnceLock::new(),
            fields: field_schema(fields)?,
            _doc: doc,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::starlark_types::{
        alloc_file, alloc_target_with_providers, analysis_globals, default_files,
    };
    use starlark::environment::{FrozenModule, Globals, LibraryExtension, Module};
    use starlark::syntax::{AstModule, Dialect};

    fn test_globals() -> Globals {
        GlobalsBuilder::extended_by(&[LibraryExtension::StructType])
            .with(analysis_globals)
            .with(globals)
            .with(crate::template_variable_info::install)
            .build()
    }

    fn file() -> File {
        File {
            path: "pkg/input.txt".to_owned(),
            short_path: "pkg/input.txt".to_owned(),
            owner: "//pkg:input.txt".to_owned(),
            is_source: true,
        }
    }

    fn context(label: &str) -> ExtensionContext {
        ExtensionContext {
            label: crate::label::Label::parse(label, "").unwrap(),
            repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                std::path::PathBuf::from("."),
            )),
        }
    }

    fn extension(label: &str, source: &str) -> anyhow::Result<FrozenModule> {
        Module::with_temp_heap(|module| {
            module.set("file", alloc_file(module.heap(), file()));
            let context = context(label);
            let ast = AstModule::parse(label, source.to_owned(), &Dialect::Standard).unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast, &test_globals())
                    .map_err(|e| anyhow!("{e}"))?;
            }
            module.freeze().map_err(|e| anyhow!("{e:?}"))
        })
    }

    fn pass(source: &str) {
        extension("//defs:providers.bzl", &format!("{source}\n")).unwrap();
    }

    fn fail(source: &str, message: &str) {
        let error = extension("//defs:providers.bzl", source)
            .unwrap_err()
            .to_string();
        assert!(error.contains(message), "expected {message:?}: {error}");
    }

    #[test]
    fn fields_are_optional_schema_checked_and_immutable() {
        pass(
            "P = provider('docs', fields=['a', 'b'])\nx = P(a=1)\n(x.a == 1 and not hasattr(x, 'b')) or fail('fields')\n(type(x) == 'struct') or fail('type')\n(x == P(a=1) and {x: 7}[P(a=1)] == 7) or fail('equality')",
        );
        pass(
            "P = provider(fields=('a', 'b'))\nx = P()\nQ = provider(fields={'a': 'documentation'})\ny = Q(a=[])\ny.a.append(1)\n(y.a == [1]) or fail('analysis payload mutation')",
        );
        pass("P = provider()\nx = P(arbitrary=42)\n(x.arbitrary == 42) or fail('open schema')");
        fail("P = provider(fields=['x'])\nx = P(y=1)", "unknown field");
        fail("P = provider(fields=[])\nx = P(x=1)", "unknown field");
        fail("P = provider()\nx = P(1)", "positional");
        fail("P = provider()\nx = P(x=1)\nx.x = 2", "not supported");
        fail("P = provider()\nx = P().missing", "missing");
        fail("provider()()", "exported name");
    }

    #[test]
    fn invalid_definitions_and_non_extension_calls_fail() {
        for (source, message) in [
            ("P = provider(init=len)", "unsupported"),
            ("P = provider(unknown=True)", "unknown"),
            ("P = provider(fields=[1])", "strings"),
            ("P = provider(fields={'x': 1})", "documentation"),
            ("P = provider(fields={1: 'doc'})", "strings"),
            ("P = provider(fields='x')", "list/tuple"),
            ("P = provider(fields=['x', 'x'])", "duplicate"),
            ("P = provider(doc=1)", "doc must be"),
        ] {
            fail(source, message);
        }
        Module::with_temp_heap(|module| {
            let ast =
                AstModule::parse("BUILD", "P = provider()".to_owned(), &Dialect::Standard).unwrap();
            let error = Evaluator::new(&module)
                .eval_module(ast, &test_globals())
                .unwrap_err()
                .to_string();
            assert!(error.contains("loading a .bzl extension"), "{error}");
        });
    }

    #[test]
    fn independent_loads_share_first_export_identity_and_relative_labels() {
        let first = extension(
            "//defs:providers.bzl",
            "P = provider()\nAlias = P\nx = P(file=file)\nlabel = Label(':local')",
        )
        .unwrap();
        let second = extension("//defs:providers.bzl", "P = provider()\nAlias = P").unwrap();
        let distinct = extension("//elsewhere:providers.bzl", "P = provider()").unwrap();
        let p = first.get("P").unwrap();
        let alias = first.get("Alias").unwrap();
        let p2 = second.get("P").unwrap();
        let q = distinct.get("P").unwrap();
        let identity = provider_identity(p.value()).unwrap();
        assert_eq!(identity, "//defs:providers.bzl%P");
        assert_eq!(provider_identity(alias.value()).unwrap(), identity);
        assert_eq!(provider_identity(p2.value()).unwrap(), identity);
        assert_ne!(provider_identity(q.value()).unwrap(), identity);
        assert_eq!(
            unpack_label(first.get("label").unwrap().value()).unwrap(),
            "//defs:local"
        );
        assert_eq!(
            instance_identity(first.get("x").unwrap().value()).unwrap(),
            identity
        );
        assert!(provider_identity(first.get("x").unwrap().value()).is_none());
        assert!(instance_identity(p.value()).is_none());
    }

    #[test]
    fn returned_providers_validate_duplicates_and_default_info() {
        let definitions =
            extension("//defs:providers.bzl", "P = provider()\nQ = provider()").unwrap();
        Module::with_temp_heap(|module| {
            module.import_public_symbols(&definitions);
            for (source, count) in [
                ("None", 0),
                ("[]", 0),
                ("DefaultInfo()", 0),
                ("P()", 1),
                ("[P(), Q(), DefaultInfo()]", 2),
                ("(P(),)", 1),
            ] {
                let ast =
                    AstModule::parse("impl.star", source.to_owned(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                assert_eq!(returned_custom_providers(result).unwrap().len(), count);
                assert_eq!(default_files(result).unwrap(), None);
            }
            for source in ["[P(), P()]", "[P]", "[struct(x=1)]", "[1]"] {
                let ast =
                    AstModule::parse("impl.star", source.to_owned(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                assert!(returned_custom_providers(result).is_err(), "{source}");
            }
        });
    }

    #[test]
    fn template_variable_return_gates_nested_payloads_and_default_outputs() {
        let definitions = extension(
            "//defs:providers.bzl",
            "P = provider()\nT = platform_common.TemplateVariableInfo",
        )
        .unwrap();
        Module::with_temp_heap(|module| {
            module.import_public_symbols(&definitions);
            module.set("file", alloc_file(module.heap(), file()));
            for (source, count, files) in [
                ("T({'FLAG': 'seed'})", 1, None),
                ("[T({})]", 1, None),
                ("(T({}),)", 1, None),
                ("[P(), T({}), DefaultInfo()]", 2, None),
                ("[DefaultInfo(files=depset()), T({}), P()]", 2, Some(vec![])),
                (
                    "[T({}), DefaultInfo(files=depset([file])), P()]",
                    2,
                    Some(vec![file()]),
                ),
            ] {
                let ast =
                    AstModule::parse("impl.star", source.to_owned(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                assert_eq!(
                    returned_custom_providers(result).unwrap().len(),
                    count,
                    "{source}"
                );
                assert_eq!(default_files(result).unwrap(), files, "{source}");
                // Analysis uses None to preserve predeclared outputs, not an
                // empty file set merely because another provider was returned.
                assert_eq!(
                    default_files(result)
                        .unwrap()
                        .unwrap_or_else(|| vec![file()]),
                    files.unwrap_or_else(|| vec![file()])
                );
            }
            for source in ["[T({}), T({})]", "x = T({}); [x, x]"] {
                let ast =
                    AstModule::parse("impl.star", source.to_owned(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                let error = returned_custom_providers(result).unwrap_err().to_string();
                assert!(
                    error.contains("builtin:TemplateVariableInfo more than once"),
                    "{error}"
                );
            }
            for source in ["T", "[T]", "struct(variables={})", "[struct(variables={})]"] {
                let ast =
                    AstModule::parse("impl.star", source.to_owned(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                assert!(returned_custom_providers(result).is_err(), "{source}");
                assert!(default_files(result).is_err(), "{source}");
            }
            let ast =
                AstModule::parse("impl.star", "T({'x': file})".to_owned(), &Dialect::Standard)
                    .unwrap();
            assert!(
                Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .is_err()
            );
        });
        let frozen = extension(
            "//defs:nested.bzl",
            "P = provider()\nT = platform_common.TemplateVariableInfo\nx = P(nested=[T({'z': 'v'}), {'k': struct(info=T({}))}], file=file)\nempty = P(nested=(T({}),))",
        ).unwrap();
        let nested = frozen.get("x").unwrap();
        let empty = frozen.get("empty").unwrap();
        drop(frozen);
        assert_eq!(provider_files(nested.value()).unwrap(), vec![file()]);
        assert!(provider_files(empty.value()).unwrap().is_empty());
        assert_ne!(
            provider_identity(definitions.get("P").unwrap().value()).as_deref(),
            Some(crate::template_variable_info::PROVIDER_ID)
        );
    }

    #[test]
    fn nested_file_payloads_cycles_depth_and_unknown_types() {
        let definitions = extension("//defs:providers.bzl", "P = provider()").unwrap();
        Module::with_temp_heap(|module| {
            module.import_public_symbols(&definitions);
            for (source, expected) in [
                (
                    "P(x=[file, (depset([file]),), {file: struct(nested=P(f=file))}], metadata=[None, True, 12345678901234567890, 1.5, 'x', Label('//pkg:x'), depset(['s'])])",
                    Some(vec![file()]),
                ),
                ("P(x={'k': 1})", Some(vec![])),
                ("l = []; p = P(x=l); l.append(p); p", None),
                ("P(x=len)", None),
                (
                    "def deep():\n    x = []\n    for i in range(140):\n        x = [x]\n    return P(x=x)\ndeep()",
                    None,
                ),
            ] {
                let ast =
                    AstModule::parse("impl.star", source.to_owned(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                match expected {
                    Some(expected) => assert_eq!(provider_files(result).unwrap(), expected),
                    None => assert!(provider_files(result).is_err(), "{source}"),
                }
            }
        });
    }

    #[test]
    fn frozen_payload_target_lookup_and_reowned_heap_lifetimes() {
        let definitions = extension(
            "//defs:providers.bzl",
            "P = provider()\nQ = provider()\nx = P(files=[file], nested=struct(values={'k': [1]}))",
        )
        .unwrap();
        let lookup = extension("//defs:providers.bzl", "P = provider()\nQ = provider()").unwrap();
        let instance = definitions.get("x").unwrap();
        let identity = instance_identity(instance.value()).unwrap();
        let p = lookup.get("P").unwrap();
        let q = lookup.get("Q").unwrap();
        // Drop both originating modules: OwnedFrozenValue is the only owner.
        drop(definitions);
        drop(lookup);
        let reowned = Module::with_temp_heap(|module| {
            module.set("P", module.heap().access_owned_frozen_value(&p));
            module.set("Q", module.heap().access_owned_frozen_value(&q));
            module.set(
                "target",
                alloc_target_with_providers(
                    module.heap(),
                    "//pkg:target",
                    vec![],
                    BTreeMap::from([(identity, instance)]),
                ),
            );
            let ast = AstModule::parse("use.star", "x = target[P]\n(P in target and Q not in target and DefaultInfo in target) or fail('membership')\n(x.files[0].path == 'pkg/input.txt' and x.nested.values['k'] == [1]) or fail('fields')".to_owned(), &Dialect::Standard).unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &test_globals())
                .unwrap();
            for source in [
                "target[Q]",
                "target[P()].files",
                "x.files.append(2)",
                "x.nested.values['k'].append(2)",
            ] {
                let ast =
                    AstModule::parse("use.star", source.to_owned(), &Dialect::Standard).unwrap();
                assert!(
                    Evaluator::new(&module)
                        .eval_module(ast, &test_globals())
                        .is_err(),
                    "{source}"
                );
            }
            module.freeze().unwrap()
        });
        drop(p);
        drop(q);
        assert_eq!(
            provider_files(reowned.get("x").unwrap().value()).unwrap(),
            vec![file()]
        );
    }
}
