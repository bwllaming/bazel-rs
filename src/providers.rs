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
use starlark::values::set::SetRef;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;
use starlark::values::typing::StarlarkCallable;
use starlark::values::{
    Coerce, Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkPagablePanic,
    StarlarkValue, Trace, UnpackValue, Value, ValueLike, starlark_value,
};
use starlark::{starlark_complex_value, starlark_module};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::hash::Hash;
use std::sync::OnceLock;

use crate::loading::ExtensionContext;
use crate::starlark_types::is_default_info;

pub(crate) const DEFAULT_INFO_PROVIDER_ID: &str = "builtin:DefaultInfo";

#[derive(Clone, Debug, Eq, PartialEq, Allocative, serde::Serialize)]
pub(crate) struct ProviderRequirement {
    pub id: String,
    pub display_name: String,
}

/// Only genuine, exported constructors are legal attribute requirements. Keep
/// display names separate: upstream package-group placement uses their exact,
/// case-sensitive spelling, not the canonical provider key.
///
/// Preflight this transient owned result before copying. A schema parser must
/// additionally charge each result to its cumulative budget BEFORE retaining it.
pub(crate) fn requirement_info(value: Value<'_>) -> anyhow::Result<ProviderRequirement> {
    let mut budget = crate::repository_budget::HostBudget::default();
    if let Some(constructor) = ProviderConstructor::from_value(value) {
        let name = constructor.exported.get().ok_or_else(|| anyhow!(
            "required provider must be assigned to a global exported name in its defining extension"
        ))?;
        budget
            .string(&constructor.extension)
            .map_err(anyhow::Error::msg)?;
        budget.string(name).map_err(anyhow::Error::msg)?;
        budget.string(name).map_err(anyhow::Error::msg)?;
        budget.bytes(1).map_err(anyhow::Error::msg)?;
        return Ok(ProviderRequirement {
            id: format!("{}%{name}", constructor.extension),
            display_name: name.clone(),
        });
    }
    let (id, display_name) = if crate::template_variable_info::is_constructor(value) {
        (
            crate::template_variable_info::PROVIDER_ID,
            "TemplateVariableInfo",
        )
    } else if crate::rule_declarations::is_output_group_info(value) {
        (
            crate::rule_declarations::OUTPUT_GROUP_INFO_ID,
            "OutputGroupInfo",
        )
    } else if crate::run_environment_info::is_constructor(value) {
        (
            crate::run_environment_info::PROVIDER_ID,
            "RunEnvironmentInfo",
        )
    } else if crate::instrumented_files::is_constructor(value) {
        (
            crate::instrumented_files::PROVIDER_ID,
            "InstrumentedFilesInfo",
        )
    } else if crate::package_specification::is_constructor(value) {
        (
            crate::package_specification::PROVIDER_ID,
            "PackageSpecificationInfo",
        )
    } else if crate::starlark_types::is_default_info_constructor(value) {
        (DEFAULT_INFO_PROVIDER_ID, "DefaultInfo")
    } else if let Some((id, display_name)) =
        crate::language_modules::builtin_provider_requirement(value)
    {
        budget.string(&id).map_err(anyhow::Error::msg)?;
        budget.string(display_name).map_err(anyhow::Error::msg)?;
        return Ok(ProviderRequirement {
            id,
            display_name: display_name.to_owned(),
        });
    } else {
        bail!(
            "required providers must be exported Provider constructors, got {}",
            value.get_type()
        );
    };
    budget.string(id).map_err(anyhow::Error::msg)?;
    budget.string(display_name).map_err(anyhow::Error::msg)?;
    Ok(ProviderRequirement {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
    })
}

// Only derive-generated marker/trace/coercion implementations need this lint
// exception. Owned constructor metadata contains no Starlark heap references.
#[allow(unsafe_code)]
mod values {
    use super::*;

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
    pub(super) struct ProviderConstructorGen<V> {
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub extension: String,
        // Written once by export_as, never changed by later aliases/re-exports.
        // OnceLock is Sync, so freezing this owned value is safe and preserves
        // the exact identity without a mutable Starlark-heap reference.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub exported: OnceLock<String>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub fields: Option<BTreeSet<String>>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub _doc: Option<String>,
        // Bazel StarlarkProvider.init: None, or a callable invoked with the
        // caller's exact positional and keyword arguments. (Option is not
        // Coerce, so None is the absent sentinel, exactly as in provider().)
        pub init: V,
    }

    /// Bazel StarlarkProvider.RawConstructor: bypasses `init` and shares the
    /// provider's identity, schema and instance type.
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
    pub(super) struct RawConstructorGen<V> {
        pub provider: V,
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
        /// `Info.getCreationLocation()`: the provider call site
        /// (StarlarkProvider.fastcall uses `thread.getCallerLocation()`).
        /// Never part of equality, hashing or repr.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        #[allocative(skip)]
        #[starlark_pagable(skip)]
        pub location: Option<starlark::codemap::FileSpan>,
    }
}
use values::{ProviderConstructorGen, ProviderInstanceGen, RawConstructorGen};

starlark_complex_value!(ProviderConstructor);
starlark_complex_value!(ProviderInstance);
starlark_complex_value!(RawConstructor);

impl<V> ProviderConstructorGen<V> {
    fn identity(&self) -> Option<String> {
        self.exported
            .get()
            .map(|name| format!("{}%{name}", self.extension))
    }
}

/// StarlarkProvider.repr (packages/StarlarkProvider.java:528-530), exported
/// or not; str() is repr().
impl<V> fmt::Display for ProviderConstructorGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<provider>")
    }
}

impl<'v, V: ValueLike<'v>> ProviderConstructorGen<V> {
    /// Native instances carry the exported key, so construction still requires
    /// export (Bazel would name an unexported provider `<no name>`).
    fn exported_key(&self) -> starlark::Result<(String, String)> {
        match (self.identity(), self.exported.get()) {
            (Some(identity), Some(name)) => Ok((identity, name.clone())),
            _ => Err(starlark::Error::new_other(anyhow!(
                "provider must be assigned to a global exported name before constructing instances"
            ))),
        }
    }

    /// Bazel RawArgumentProcessor: the default constructor `c(**kwargs)`.
    /// `callable` is the provider's or raw constructor's getName().
    fn raw_call(
        &self,
        callable: &str,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let (identity, name) = self.exported_key()?;
        if args.positions(eval.heap())?.next().is_some() {
            return Err(starlark::Error::new_other(anyhow!(
                "{callable}: unexpected positional arguments"
            )));
        }
        let named = args.names_map()?;
        let entries = named
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), *value))
            .collect();
        self.instantiate(identity, name, entries, eval)
    }

    /// Bazel ArgumentProcessorWithInit: forward the exact arguments to init,
    /// cast its result to dict<string, unknown> and build as if by `c(**d)`.
    fn init_call(
        &self,
        init: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let (identity, name) = self.exported_key()?;
        let positional = args.positions(eval.heap())?.collect::<Vec<_>>();
        let named = args.names_map()?;
        let named = named
            .iter()
            .map(|(name, value)| (name.as_str(), *value))
            .collect::<Vec<_>>();
        let result = eval.eval_function(init, &positional, &named)?;
        const WHAT: &str = "return value of provider init()";
        let dict = DictRef::from_value(result).ok_or_else(|| {
            starlark::Error::new_other(anyhow!("got {} for '{WHAT}', want dict", result.get_type()))
        })?;
        let mut entries = Vec::with_capacity(dict.len());
        for (key, value) in dict.iter() {
            let Some(key) = key.unpack_str() else {
                return Err(starlark::Error::new_other(anyhow!(
                    "got dict<{}, {}> for '{WHAT}', want dict<string, unknown>",
                    key.get_type(),
                    value.get_type()
                )));
            };
            entries.push((key.to_owned(), value));
        }
        drop(dict);
        self.instantiate(identity, name, entries, eval)
    }

    /// Bazel StarlarkInfoFactory addNamedArg + createFromArgs/createFromMap.
    fn instantiate(
        &self,
        identity: String,
        name: String,
        entries: Vec<(String, Value<'v>)>,
        eval: &Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        let mut seen = BTreeSet::new();
        let mut unexpected = Vec::new();
        let mut fields = Vec::with_capacity(entries.len());
        for (field, value) in entries {
            if !seen.insert(field.clone()) {
                return Err(starlark::Error::new_other(anyhow!(
                    "got multiple values for parameter {field} in call to instantiate provider {name}"
                )));
            }
            if self
                .fields
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(&field))
            {
                unexpected.push(field);
            } else {
                fields.push((field, value));
            }
        }
        if !unexpected.is_empty() {
            return Err(starlark::Error::new_other(anyhow!(
                "got unexpected field{} '{}' in call to instantiate provider {name}",
                if unexpected.len() > 1 { "s" } else { "" },
                unexpected.join("', '")
            )));
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(heap.alloc(ProviderInstanceGen {
            identity,
            name,
            fields,
            location: eval.call_stack_top_location(),
        }))
    }
}

#[starlark_value(type = "Provider")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ProviderConstructorGen<V>
where
    Self: ProvidesStaticType<'v>,
{
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
        let init = self.init.to_value();
        if init.is_none() {
            let name = self.exported.get().map_or("<no name>", String::as_str);
            self.raw_call(name, args, eval)
        } else {
            self.init_call(init, args, eval)
        }
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

/// Bazel's RawConstructor has no StarlarkBuiltin annotation or repr override,
/// so type() is its Java simple name and repr() is StarlarkValue's default.
impl<V> fmt::Display for RawConstructorGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "<unknown object com.google.devtools.build.lib.packages.StarlarkProvider$RawConstructor>",
        )
    }
}

#[starlark_value(type = "RawConstructor")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RawConstructorGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let provider = ProviderConstructor::from_value(self.provider.to_value())
            .expect("raw constructor always wraps its provider");
        // Bazel RawConstructor.getName(), derived from the provider's export.
        let callable = match provider.exported.get() {
            Some(name) => format!("<raw constructor for {name}>"),
            None => "<raw constructor>".to_owned(),
        };
        provider.raw_call(&callable, args, eval)
    }
}

/// Bazel's unknown-field text of a struct-like value, `'NAME' value has no field
/// or method 'x'` followed by `Available attributes:` and the field names in
/// natural order (packages/StructImpl.java:85-95).
pub(crate) fn unknown_field_message<'a>(
    name: &str,
    attribute: &str,
    fields: impl Iterator<Item = &'a str>,
) -> String {
    let mut fields: Vec<&str> = fields.collect();
    fields.sort_unstable();
    format!(
        "'{name}' value has no field or method '{attribute}'\nAvailable attributes: {}",
        fields.join(", ")
    )
}

/// StarlarkInfo inherits StructImpl.repr (packages/StructImpl.java:139-153):
/// `struct(` then the present fields sorted by name as `name = repr`.
impl<'v, V: ValueLike<'v>> fmt::Display for ProviderInstanceGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("struct(")?;
        for (index, (name, value)) in self.fields.iter().enumerate() {
            if index != 0 {
                f.write_str(", ")?;
            }
            write!(f, "{name} = {}", value.to_value())?;
        }
        f.write_str(")")
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

    /// StarlarkInfo: StructImpl.getErrorMessageForUnknownField
    /// (packages/StructImpl.java:85-95) with StarlarkProvider's text for an
    /// exported provider (packages/StarlarkProvider.java:465-469).
    fn unknown_attribute_message(&self, attribute: &str) -> Option<String> {
        Some(unknown_field_message(
            &self.name,
            attribute,
            self.fields.iter().map(|(name, _)| name.as_str()),
        ))
    }

    fn collect_repr(&self, collector: &mut String) {
        collector.push_str("struct(");
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
        collector.push_str("struct(...)");
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

/// The parts of a Starlark provider instance (N49 depset snapshots,
/// crate::depset_items): its provider key and display name, its fields in
/// name order and its creation location.
pub(crate) type InstanceParts<'v> = (
    String,
    String,
    Vec<(String, Value<'v>)>,
    Option<starlark::codemap::FileSpan>,
);

pub(crate) fn instance_parts<'v>(value: Value<'v>) -> Option<InstanceParts<'v>> {
    let instance = ProviderInstance::from_value(value)?;
    Some((
        instance.identity.clone(),
        instance.name.clone(),
        instance
            .fields
            .iter()
            .map(|(name, value)| (name.clone(), *value))
            .collect(),
        instance.location.clone(),
    ))
}

/// A provider instance with the given parts (fields sorted by name).
pub(crate) fn alloc_instance<'v>(
    heap: Heap<'v>,
    identity: String,
    name: String,
    fields: Vec<(String, Value<'v>)>,
    location: Option<starlark::codemap::FileSpan>,
) -> Value<'v> {
    heap.alloc(ProviderInstanceGen {
        identity,
        name,
        fields,
        location,
    })
}

/// Stable identity of an exported constructor, never of an instance.
pub fn provider_identity(value: Value<'_>) -> Option<String> {
    if crate::rule_declarations::is_output_group_info(value) {
        return Some(crate::output_group_info::PROVIDER_ID.to_owned());
    }
    if crate::run_environment_info::is_constructor(value) {
        return Some(crate::run_environment_info::PROVIDER_ID.to_owned());
    }
    if crate::instrumented_files::is_constructor(value) {
        return Some(crate::instrumented_files::PROVIDER_ID.to_owned());
    }
    if crate::package_specification::is_constructor(value) {
        return Some(crate::package_specification::PROVIDER_ID.to_owned());
    }
    if crate::template_variable_info::is_constructor(value) {
        return Some(crate::template_variable_info::PROVIDER_ID.to_owned());
    }
    // platform_common.ConstraintSettingInfo / ConstraintValueInfo /
    // PlatformInfo: native providers of the platform rules
    // (crate::platform_providers).
    if let Some((id, _)) = crate::language_modules::builtin_provider_requirement(value)
        && [
            crate::platform_providers::CONSTRAINT_SETTING_ID,
            crate::platform_providers::CONSTRAINT_VALUE_ID,
            crate::platform_providers::PLATFORM_ID,
            crate::toolchain_info::PROVIDER_ID,
            crate::feature_flags::PROVIDER_ID,
            crate::execution_info::PROVIDER_ID,
        ]
        .contains(&id.as_str())
    {
        return Some(id);
    }
    ProviderConstructor::from_value(value)?.identity()
}

/// Stable identity of an instance, never of a constructor or ordinary struct.
pub fn instance_identity(value: Value<'_>) -> Option<String> {
    if crate::output_group_info::is_instance(value) {
        return Some(crate::output_group_info::PROVIDER_ID.to_owned());
    }
    if crate::run_environment_info::is_instance(value) {
        return Some(crate::run_environment_info::PROVIDER_ID.to_owned());
    }
    if crate::instrumented_files::is_instance(value) {
        return Some(crate::instrumented_files::PROVIDER_ID.to_owned());
    }
    if crate::package_specification::is_instance(value) {
        return Some(crate::package_specification::PROVIDER_ID.to_owned());
    }
    if crate::template_variable_info::is_instance(value) {
        return Some(crate::template_variable_info::PROVIDER_ID.to_owned());
    }
    if crate::toolchain_info::is_instance(value) {
        return Some(crate::toolchain_info::PROVIDER_ID.to_owned());
    }
    if crate::feature_flags::is_instance(value) {
        return Some(crate::feature_flags::PROVIDER_ID.to_owned());
    }
    if crate::execution_info::is_instance(value) {
        return Some(crate::execution_info::PROVIDER_ID.to_owned());
    }
    Some(ProviderInstance::from_value(value)?.identity.clone())
}

/// The call site that created a custom-provider instance (Bazel
/// `Info.getCreationLocation()`), if it was created by a Starlark call.
pub(crate) fn instance_creation_location(value: Value<'_>) -> Option<starlark::codemap::FileSpan> {
    ProviderInstance::from_value(value)?.location.clone()
}

/// Borrow genuine custom-provider fields, already sorted by name. The typed
/// downcast covers mutable and frozen instances without reflecting attributes or
/// copying names/values. Native providers and ordinary structs are not adapters.
pub(crate) fn json_fields<'v>(
    value: Value<'v>,
) -> Option<impl ExactSizeIterator<Item = (&'v str, Value<'v>)> + 'v> {
    Some(
        ProviderInstance::from_value(value)?
            .fields
            .iter()
            .map(|(name, value)| (name.as_str(), *value)),
    )
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
        if crate::package_specification::is_instance(value) {
            bail!(
                "returning PackageSpecificationInfo from a custom rule is unsupported; obtain it from an actual package_group target"
            );
        }
        if providers.insert(identity.clone(), value).is_some() {
            bail!("implementation returned provider {identity} more than once");
        }
    }
    Ok(providers)
}

/// Provider.Key names of the returned declared providers in return order, as
/// RuleConfiguredTarget.debugPrint lists them (StarlarkProvider.Key.toString
/// is the exported name, packages/StarlarkProvider.java:655-657;
/// BuiltinProvider.Key.toString the provider name, BuiltinProvider.java:153-155).
/// DefaultInfo is not a declared provider of the target.
pub fn returned_provider_key_names(result: Value<'_>) -> anyhow::Result<Vec<String>> {
    if result.is_none() {
        return Ok(Vec::new());
    }
    let values = if is_default_info(result) || instance_identity(result).is_some() {
        vec![result]
    } else {
        sequence(result)?
    };
    Ok(values
        .into_iter()
        .filter_map(instance_identity)
        // A returned OutputGroupInfo is decomposed into the target's output
        // groups (RuleConfiguredTargetBuilder.addStarlarkDeclaredProvider);
        // debugPrint lists the framework's OutputGroupInfo last.
        .filter(|identity| identity != crate::output_group_info::PROVIDER_ID)
        .map(|identity| match identity.rsplit_once('%') {
            Some((_, name)) => name.to_owned(),
            None => identity
                .strip_prefix("builtin:")
                .unwrap_or(&identity)
                .to_owned(),
        })
        .collect())
}

/// The Starlark types of the analysis-bound values the native profile
/// allocates without freeze support (`Heap::alloc_complex_no_freeze`): rule,
/// aspect and subrule contexts, their action factories, `Args`,
/// `TemplateDict`, `ctx.fragments` and the aspect's rule attributes. Bazel
/// lets a provider hold them; a provider here is frozen with its target's
/// analysis, so they cannot cross it.
const ANALYSIS_BOUND_TYPES: [&str; 7] = [
    "ctx",
    "subrule_ctx",
    "actions",
    "Args",
    "TemplateDict",
    "fragments",
    "StarlarkAttributesCollection",
];

/// Reject an analysis-bound value (ANALYSIS_BOUND_TYPES) among the returned
/// providers' payloads before they are frozen, with the pre-W1b provider-file
/// walk's `unsupported provider payload type T`. Only this analysis's own
/// (unfrozen) values are visited — fields, list/tuple/set/dict elements and
/// struct fields; frozen values (a dependency's providers) cannot hold an
/// unfrozen value — so the check is linear in the target's own payload. A
/// value captured by a closure is not visited; its freeze fails instead
/// ([`unsupported_payload_freeze`]).
pub(crate) fn check_freezable_payloads<'v>(
    providers: impl IntoIterator<Item = Value<'v>>,
) -> anyhow::Result<()> {
    let mut visited = HashSet::new();
    let mut stack: Vec<Value<'v>> = providers.into_iter().collect();
    stack.reverse();
    while let Some(value) = stack.pop() {
        if value.unpack_frozen().is_some() || !visited.insert(value.identity()) {
            continue;
        }
        let kind = value.get_type();
        if ANALYSIS_BOUND_TYPES.contains(&kind) {
            bail!("unsupported provider payload type {kind}");
        }
        let mut children: Vec<Value<'v>> = Vec::new();
        if let Some(provider) = ProviderInstance::from_value(value) {
            children.extend(provider.fields.iter().map(|(_, field)| *field));
        } else if let Some(list) = ListRef::from_value(value) {
            children.extend(list.iter());
        } else if let Some(tuple) = TupleRef::from_value(value) {
            children.extend(tuple.iter());
        } else if let Some(set) = SetRef::unpack_value_opt(value) {
            children.extend(set.iter());
        } else if let Some(dict) = DictRef::from_value(value) {
            children.extend(dict.iter().flat_map(|(key, child)| [key, child]));
        } else if let Some(structure) = StructRef::from_value(value) {
            children.extend(structure.iter().map(|(_, child)| child));
        }
        stack.extend(children.into_iter().rev());
    }
    Ok(())
}

/// A provider freeze that failed after [`check_freezable_payloads`] (an
/// analysis-bound value captured by a closure): the same rejection class,
/// with the freezer's message.
pub(crate) fn unsupported_payload_freeze(error: starlark::values::FreezeError) -> anyhow::Error {
    anyhow!("unsupported provider payload: {}", error.err_msg)
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
        let doc = if doc.is_none() {
            None
        } else {
            Some(
                doc.unpack_str()
                    .ok_or_else(|| anyhow!("provider doc must be a string or None"))?
                    .to_owned(),
            )
        };
        // Bazel binds and type-checks every parameter (doc, fields, init)
        // before provider() validates field names, so a non-callable init is
        // reported before element-level schema errors.
        if !(fields.is_none()
            || DictRef::from_value(fields).is_some()
            || ListRef::from_value(fields).is_some()
            || TupleRef::from_value(fields).is_some())
        {
            bail!(
                "provider fields must be a list/tuple of strings or a string-to-documentation dict"
            );
        }
        if !init.is_none() && <StarlarkCallable<'v>>::unpack_value_opt(init).is_none() {
            bail!(
                "in call to provider(), parameter 'init' got value of type '{}', want 'callable or NoneType'",
                init.get_type()
            );
        }
        let provider = eval.heap().alloc(ProviderConstructorGen {
            extension: context.label.to_string(),
            exported: OnceLock::new(),
            fields: field_schema(fields)?,
            _doc: doc,
            init,
        });
        if init.is_none() {
            return Ok(provider);
        }
        // StarlarkRuleClassFunctions.provider: Tuple.of(provider, raw constructor).
        let raw = eval.heap().alloc(RawConstructorGen { provider });
        Ok(eval.heap().alloc((provider, raw)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::starlark_types::{
        File, alloc_file, alloc_target_with_providers, analysis_globals, default_files,
        unpack_label,
    };
    use starlark::environment::{FrozenModule, Globals, LibraryExtension, Module};
    use starlark::syntax::{AstModule, Dialect};

    fn test_globals() -> Globals {
        GlobalsBuilder::extended_by(&[LibraryExtension::StructType, LibraryExtension::SetType])
            .with(analysis_globals)
            .with(globals)
            .with(crate::template_variable_info::install)
            .with(crate::run_environment_info::install)
            .with(crate::instrumented_files::install)
            .with(crate::package_specification::install)
            .build()
    }

    fn file() -> File {
        File {
            is_directory: false,
            is_symlink: false,
            path: "pkg/input.txt".to_owned(),
            short_path: "pkg/input.txt".to_owned(),
            owner: "//pkg:input.txt".to_owned(),
            is_source: true,
        }
    }

    /// The File held by field `name` of a provider instance.
    fn file_field(value: Value<'_>, name: &str) -> Option<File> {
        let (_, _, fields, _) = instance_parts(value)?;
        fields
            .into_iter()
            .find(|(field, _)| field == name)
            .and_then(|(_, value)| File::from_value(value).cloned())
    }

    #[test]
    fn run_environment_registry_requirements_returns_and_nested_target_transport() {
        let definitions = extension("//defs:providers.bzl", "P = provider()\nR = RunEnvironmentInfo\nAlias = R\ninfo = R({'z': 'v'}, ['HOME', 'HOME'])\nx = P(info=info, nested=[{'k': struct(info=(info,))}], file=file)").unwrap();
        let r = definitions.get("R").unwrap();
        let alias = definitions.get("Alias").unwrap();
        let p = definitions.get("P").unwrap();
        let info = definitions.get("info").unwrap();
        let x = definitions.get("x").unwrap();
        let id = crate::run_environment_info::PROVIDER_ID;
        assert_eq!(provider_identity(r.value()).as_deref(), Some(id));
        assert_eq!(provider_identity(alias.value()).as_deref(), Some(id));
        assert_eq!(instance_identity(info.value()).as_deref(), Some(id));
        assert!(instance_identity(r.value()).is_none());
        assert!(provider_identity(info.value()).is_none());
        assert!(requirement_info(info.value()).is_err());
        assert_eq!(
            requirement_info(r.value()).unwrap(),
            ProviderRequirement {
                id: id.into(),
                display_name: "RunEnvironmentInfo".into(),
            }
        );
        assert_eq!(file_field(x.value(), "file"), Some(file()));
        Module::with_temp_heap(|module| {
            module.import_public_symbols(&definitions);
            for source in [
                "info",
                "[info]",
                "(info,)",
                "[DefaultInfo(), info]",
                "[info, DefaultInfo(files=depset())]",
            ] {
                let ast =
                    AstModule::parse("return.bzl", source.into(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                let providers = returned_custom_providers(result).unwrap();
                assert_eq!(providers.len(), 1);
                assert!(providers.contains_key(id));
            }
            for source in ["[info, R()]", "[info, info]"] {
                let ast =
                    AstModule::parse("return.bzl", source.into(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                assert!(
                    returned_custom_providers(result)
                        .unwrap_err()
                        .to_string()
                        .contains("more than once")
                );
            }
            for source in [
                "R",
                "'RunEnvironmentInfo'",
                "'builtin:RunEnvironmentInfo'",
                "R()",
                "struct(environment={}, inherited_environment=[])",
            ] {
                let ast = AstModule::parse("key.bzl", source.into(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                assert_eq!(requirement_info(result).is_ok(), source == "R", "{source}");
            }
        });
        let custom_id = instance_identity(x.value()).unwrap();
        drop(definitions);
        let consumer = Module::with_temp_heap(|module| {
            module.set("R", module.heap().access_owned_frozen_value(&r));
            module.set("P", module.heap().access_owned_frozen_value(&p));
            module.set(
                "target",
                alloc_target_with_providers(
                    module.heap(),
                    "//pkg:wrapped",
                    vec![],
                    BTreeMap::from([(custom_id, x)]),
                ),
            );
            // A synthetic native slot tests key dispatch only, not rule-return
            // legality. Ordinary analysis must reject creating such a slot.
            module.set(
                "native_target",
                alloc_target_with_providers(
                    module.heap(),
                    "//pkg:native",
                    vec![],
                    BTreeMap::from([(id.into(), info)]),
                ),
            );
            let ast = AstModule::parse("consumer.bzl", r#"
(P in target and R not in target) or fail("no synthetic native membership")
x = target[P]
(x.info.environment == {"z": "v"} and x.info.inherited_environment == ["HOME", "HOME"]) or fail("nested metadata")
copy = x.info.environment
copy["z"] = "consumer"
names = x.info.inherited_environment
names.append("consumer")
(x.info.environment == {"z": "v"} and x.info.inherited_environment == ["HOME", "HOME"]) or fail("fresh projections")
(R in native_target and native_target[R] == x.info) or fail("native key dispatch")
"#.into(), &Dialect::Standard).unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &test_globals())
                .unwrap();
            for source in [
                "R() not in target",
                "'RunEnvironmentInfo' not in native_target",
            ] {
                let ast =
                    AstModule::parse("wrong_key.bzl", source.into(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("Target membership requires an exported provider constructor"),
                    "{source}: {error}"
                );
            }
            for source in [
                "target[R]",
                "target[R()]",
                "native_target['RunEnvironmentInfo']",
                "native_target[R()]",
            ] {
                let ast =
                    AstModule::parse("wrong_key.bzl", source.into(), &Dialect::Standard).unwrap();
                assert!(
                    Evaluator::new(&module)
                        .eval_module(ast, &test_globals())
                        .is_err(),
                    "{source}"
                );
            }
            module.freeze().unwrap()
        });
        drop(r);
        drop(p);
        drop(alias);
        let reowned = consumer.get("x").unwrap();
        drop(consumer);
        assert_eq!(file_field(reowned.value(), "file"), Some(file()));
    }

    fn context(label: &str) -> ExtensionContext {
        ExtensionContext {
            byte_strings: false,
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
            // The .bzl loading dialect (src/loading.rs): keyword-only parameters.
            let dialect = Dialect {
                enable_keyword_only_arguments: true,
                ..Dialect::Standard
            };
            let ast = AstModule::parse(label, source.to_owned(), &dialect).unwrap();
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

    #[test]
    fn package_specification_constructor_is_a_real_noncallable_key() {
        pass(
            "P = PackageSpecificationInfo\n(type(P) == 'Provider' and repr(P) == '<function PackageSpecificationInfo>' and dir(P) == []) or fail('native key')\n(P == PackageSpecificationInfo and {P: 1}[PackageSpecificationInfo] == 1 and P != DefaultInfo) or fail('identity')",
        );
        // Starlark.call on a non-callable (Starlark.java:904).
        fail(
            "PackageSpecificationInfo()",
            "'Provider' object is not callable",
        );
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
        fail(
            "P = provider(fields=['x'])\nx = P(y=1)",
            "got unexpected field 'y' in call to instantiate provider P",
        );
        fail(
            "P = provider(fields=[])\nx = P(x=1, z=2)",
            "got unexpected fields 'x', 'z' in call to instantiate provider P",
        );
        fail(
            "P = provider()\nx = P(1)",
            "P: unexpected positional arguments",
        );
        fail(
            "P = provider()\nx = P(x=1)\nx.x = 2",
            "struct value does not support field assignment",
        );
        fail(
            "P = provider()\nx = P().missing",
            "'P' value has no field or method 'missing'\nAvailable attributes: ",
        );
        // StarlarkInfo is not Comparable (Starlark.java:492-518).
        fail(
            "P = provider()\nx = P(a=1) < P(a=2)",
            "unsupported comparison: struct <=> struct",
        );
        fail("provider()()", "exported name");
    }

    const INIT_PROVIDER: &str = r#"
def _init(name, *, extra = None, **unused):
    out = {"name": name, "upper": name.upper()}
    if extra != None:
        out["extra"] = extra
    return out

P, _new_p = provider(
    doc = "Provider with a custom initializer.",
    fields = {"name": "Name.", "upper": "Derived.", "extra": "Optional."},
    init = _init,
)
a = P("x")
b = P(name = "y", extra = [1])
args = ["z"]
kwargs = {"extra": 3}
d = P(*args, **kwargs)
c = _new_p(name = "raw")
"#;

    #[test]
    fn provider_init_forwards_arguments_and_raw_constructor_bypasses_it() {
        let definitions = extension(
            "//defs:providers.bzl",
            &format!(
                "{INIT_PROVIDER}{}",
                r#"
(a.name == "x" and a.upper == "X" and not hasattr(a, "extra")) or fail("positional init")
(b.name == "y" and b.upper == "Y" and b.extra == [1]) or fail("keyword init")
(d.name == "z" and d.upper == "Z" and d.extra == 3) or fail("star arguments")
(c.name == "raw" and not hasattr(c, "upper")) or fail("raw bypass")
(type(P) == "Provider" and type(_new_p) == "RawConstructor") or fail("types")
(repr(_new_p) == "<unknown object com.google.devtools.build.lib.packages.StarlarkProvider$RawConstructor>") or fail("raw repr")
(str(_new_p) == repr(_new_p)) or fail("raw str")
(repr(P) == "<provider>" and str(P) == "<provider>") or fail("provider repr")
(repr(provider()) == "<provider>") or fail("unexported provider repr")
(repr(a) == 'struct(name = "x", upper = "X")' and str(a) == repr(a)) or fail("instance repr " + repr(a))
(repr([c]) == '[struct(name = "raw")]') or fail("nested instance repr")
(repr(struct(z = 1, a = c)) == 'struct(a = struct(name = "raw"), z = 1)') or fail("struct repr " + repr(struct(z = 1, a = c)))
(type(a) == "struct" and type(c) == "struct") or fail("instance types")
(a == _new_p(name = "x", upper = "X") and a != c) or fail("shared instance type")
(_new_p == _new_p and _new_p != P) or fail("raw identity")
[Q, _new_q] = provider(init = lambda **kw: dict(kw, computed = len(kw)))
q = Q(one = 1, two = 2)
(q.one == 1 and q.two == 2 and q.computed == 2 and not hasattr(_new_q(one = 1), "computed")) or fail("schemaless init")
RAW_P = _new_p
RAW_Q = _new_q
"#
            ),
        )
        .unwrap();
        let identity = "//defs:providers.bzl%P";
        assert_eq!(
            provider_identity(definitions.get("P").unwrap().value()).as_deref(),
            Some(identity)
        );
        assert_eq!(
            requirement_info(definitions.get("P").unwrap().value()).unwrap(),
            ProviderRequirement {
                id: identity.into(),
                display_name: "P".into(),
            }
        );
        assert_eq!(
            provider_identity(definitions.get("Q").unwrap().value()).as_deref(),
            Some("//defs:providers.bzl%Q")
        );
        for raw in ["RAW_P", "RAW_Q"] {
            let value = definitions.get(raw).unwrap();
            assert!(provider_identity(value.value()).is_none(), "{raw}");
            assert!(instance_identity(value.value()).is_none(), "{raw}");
            assert!(requirement_info(value.value()).is_err(), "{raw}");
        }
        for instance in ["a", "b", "c", "d"] {
            assert_eq!(
                instance_identity(definitions.get(instance).unwrap().value()).as_deref(),
                Some(identity),
                "{instance}"
            );
        }
        // Both construction paths are found by the same exported key, also
        // after the defining module is dropped and the values are reowned.
        let instance = definitions.get("c").unwrap();
        let p = definitions.get("P").unwrap();
        let raw = definitions.get("RAW_P").unwrap();
        drop(definitions);
        Module::with_temp_heap(|module| {
            module.set("P", module.heap().access_owned_frozen_value(&p));
            module.set("raw", module.heap().access_owned_frozen_value(&raw));
            module.set(
                "target",
                alloc_target_with_providers(
                    module.heap(),
                    "//pkg:target",
                    vec![],
                    BTreeMap::from([(identity.to_owned(), instance)]),
                ),
            );
            let ast = AstModule::parse(
                "use.star",
                "(P in target and target[P].name == 'raw') or fail('lookup')
raw(name = 'later').name == 'later' or fail('frozen raw')
P('w').upper == 'W' or fail('frozen init')"
                    .into(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &test_globals())
                .unwrap();
            for source in ["raw in target", "target[raw]"] {
                let ast = AstModule::parse("use.star", source.into(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("exported provider constructor"),
                    "{source}: {error}"
                );
            }
        });
    }

    #[test]
    fn provider_init_errors_match_bazel() {
        const WHAT: &str = "for 'return value of provider init()'";
        for (source, message) in [
            (
                "def _i():\n    return 1\nP, _r = provider(init = _i)\nP()",
                format!("got int {WHAT}, want dict"),
            ),
            (
                "def _i():\n    return [('a', 1)]\nP, _r = provider(init = _i)\nP()",
                format!("got list {WHAT}, want dict"),
            ),
            (
                "def _i():\n    return {'ok': 1, 2: 'v'}\nP, _r = provider(init = _i)\nP()",
                format!("got dict<int, string> {WHAT}, want dict<string, unknown>"),
            ),
            (
                "def _i():\n    return {'a': 1, 'b': 2, 'c': 3}\nP, _r = provider(fields = ['a'], init = _i)\nP()",
                "got unexpected fields 'b', 'c' in call to instantiate provider P".to_owned(),
            ),
            (
                "def _i(**kw):\n    return kw\nP, _r = provider(fields = {'a': 'doc'}, init = _i)\n_r(b = 1)",
                "got unexpected field 'b' in call to instantiate provider P".to_owned(),
            ),
            (
                "def _i(*args):\n    return {}\nP, _r = provider(init = _i)\nP(1)\n_r(1)",
                "<raw constructor for P>: unexpected positional arguments".to_owned(),
            ),
            (
                "def _i(x):\n    return {}\nP, _r = provider(init = _i)\nP()",
                "x".to_owned(),
            ),
            (
                "def _i(x):\n    fail('bad init ' + x)\nP, _r = provider(init = _i)\nP('value')",
                "bad init value".to_owned(),
            ),
            (
                "P = provider(fields = [1], init = 1)",
                "parameter 'init' got value of type 'int'".to_owned(),
            ),
            (
                "P = provider(fields = 'x', init = 1)",
                "list/tuple".to_owned(),
            ),
            (
                "P = provider(init = 'callable')",
                "got value of type 'string', want 'callable or NoneType'".to_owned(),
            ),
            (
                "def _i():\n    return {}\ndef f():\n    return provider(init = _i)\nBOX = [f()]\nBOX[0][0]()",
                "exported name".to_owned(),
            ),
            (
                "def _i():\n    return {}\ndef f():\n    return provider(init = _i)\nBOX = [f()]\nBOX[0][1]()",
                "exported name".to_owned(),
            ),
        ] {
            fail(source, &message);
        }
        // Tuple-unpacked definitions export only the provider; the raw
        // constructor is not exportable and is unhashable like Bazel's.
        pass(
            "def _i():\n    return {}\nP, _r = provider(init = _i)\nP()\n_r()\n(len(provider(init = _i)) == 2) or fail('tuple')",
        );
        fail(
            "def _i():\n    return {}\nP, _r = provider(init = _i)\nx = {_r: 1}",
            "hash",
        );
        // Any callable is a legal init; a provider init returns an instance,
        // which then fails the dict cast.
        fail(
            "Inner = provider()\nOuter, _o = provider(init = Inner)\nx = Outer(a = 1)",
            "got struct for 'return value of provider init()', want dict",
        );
    }

    #[test]
    fn json_fields_borrow_sorted_live_values_and_retain_frozen_owners() {
        fn check(value: Value<'_>, expected: &str) {
            let provider = ProviderInstance::from_value(value).unwrap();
            let mut fields = json_fields(value).unwrap();
            assert_eq!(fields.len(), 2);
            let (name, child) = fields.next().unwrap();
            assert_eq!(name, "a");
            assert!(std::ptr::eq(name.as_ptr(), provider.fields[0].0.as_ptr()));
            assert_eq!(child.identity(), provider.fields[0].1.identity());
            assert_eq!(child.to_repr(), expected);
            assert_eq!(fields.len(), 1);
            let (name, child) = fields.next().unwrap();
            assert_eq!(name, "z");
            assert_eq!(child.unpack_i32(), Some(7));
            assert_eq!(fields.len(), 0);
            assert!(fields.next().is_none());
        }
        let frozen = Module::with_temp_heap(|module| {
            let context = context("//defs:json.bzl");
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                let ast = AstModule::parse(
                    "json.bzl",
                    "P = provider()\npayload = [1]\nx = P(z=7, a=payload)\nempty = P()".into(),
                    &Dialect::Standard,
                )
                .unwrap();
                eval.eval_module(ast, &test_globals()).unwrap();
                check(module.get("x").unwrap(), "[1]");
                assert_eq!(json_fields(module.get("empty").unwrap()).unwrap().len(), 0);
                let ast =
                    AstModule::parse("mutate.bzl", "payload.append(2)".into(), &Dialect::Standard)
                        .unwrap();
                eval.eval_module(ast, &test_globals()).unwrap();
                check(module.get("x").unwrap(), "[1, 2]");
            }
            module.freeze().unwrap()
        });
        let value = frozen.get("x").unwrap();
        drop(frozen);
        check(value.value(), "[1, 2]");
        let reowned = Module::with_temp_heap(|module| {
            module.set("x", module.heap().access_owned_frozen_value(&value));
            module.freeze().unwrap()
        });
        drop(value);
        let value = reowned.get("x").unwrap();
        drop(reowned);
        check(value.value(), "[1, 2]");
        Module::with_temp_heap(|module| {
            module.set("x", module.heap().access_owned_frozen_value(&value));
            let ast =
                AstModule::parse("mutate.bzl", "x.a.append(3)".into(), &Dialect::Standard).unwrap();
            assert!(
                Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .is_err()
            );
        });
    }

    #[test]
    fn json_fields_reject_constructors_structs_and_native_info() {
        let frozen = extension(
            "//defs:json.bzl",
            r#"
P = provider()
ordinary = struct(a=1)
empty = struct()
default = DefaultInfo()
run_environment = RunEnvironmentInfo()
template = platform_common.TemplateVariableInfo({})
"#,
        )
        .unwrap();
        for name in [
            "P",
            "ordinary",
            "empty",
            "default",
            "run_environment",
            "template",
        ] {
            assert!(
                json_fields(frozen.get(name).unwrap().value()).is_none(),
                "{name}"
            );
        }
        Module::with_temp_heap(|module| {
            for source in [
                "provider()",
                "struct()",
                "DefaultInfo()",
                "RunEnvironmentInfo()",
                "platform_common.TemplateVariableInfo({})",
                "PackageSpecificationInfo",
                "len",
                "None",
            ] {
                let context = context("//defs:json.bzl");
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                let ast = AstModule::parse("json.bzl", source.into(), &Dialect::Standard).unwrap();
                let value = eval.eval_module(ast, &test_globals()).unwrap();
                assert!(json_fields(value).is_none(), "{source}");
            }
            let package = crate::label::PackageId {
                repository: String::new(),
                package: "groups".into(),
            };
            let contents = crate::package_specification::parse_specs(
                &[],
                &package,
                &crate::repository::RepositorySet::new(".".into()),
            )
            .unwrap();
            let node = std::sync::Arc::new(
                crate::package_specification::GroupNode::new(std::sync::Arc::new(contents), vec![])
                    .unwrap(),
            );
            let info = crate::package_specification::alloc_info(&module.heap(), node);
            assert!(json_fields(info).is_none());
            module.set("info", info);
            let frozen = module.freeze().unwrap();
            assert!(json_fields(frozen.get("info").unwrap().value()).is_none());
        });
    }

    #[test]
    fn invalid_definitions_and_non_extension_calls_fail() {
        for (source, message) in [
            (
                "P = provider(init=1)",
                "in call to provider(), parameter 'init' got value of type 'int', want 'callable or NoneType'",
            ),
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
    fn requirements_use_exported_constructor_keys_and_exact_display_names() {
        let first = extension("//defs:providers.bzl", "NotPackageSpecificationInfo = provider()\nAlias = NotPackageSpecificationInfo\nx = Alias()\nDefault = DefaultInfo\nTemplate = platform_common.TemplateVariableInfo\nGroup = PackageSpecificationInfo").unwrap();
        let second = extension(
            "//elsewhere:providers.bzl",
            "NotPackageSpecificationInfo = provider()",
        )
        .unwrap();
        let key = first.get("NotPackageSpecificationInfo").unwrap();
        let expected = ProviderRequirement {
            id: "//defs:providers.bzl%NotPackageSpecificationInfo".into(),
            display_name: "NotPackageSpecificationInfo".into(),
        };
        assert_eq!(requirement_info(key.value()).unwrap(), expected);
        assert_eq!(
            requirement_info(first.get("Alias").unwrap().value()).unwrap(),
            expected
        );
        let other =
            requirement_info(second.get("NotPackageSpecificationInfo").unwrap().value()).unwrap();
        assert_ne!(other.id, expected.id);
        assert_eq!(other.display_name, expected.display_name);
        for (symbol, id, name) in [
            ("Default", DEFAULT_INFO_PROVIDER_ID, "DefaultInfo"),
            (
                "Template",
                crate::template_variable_info::PROVIDER_ID,
                "TemplateVariableInfo",
            ),
            (
                "Group",
                crate::package_specification::PROVIDER_ID,
                "PackageSpecificationInfo",
            ),
        ] {
            let value = first.get(symbol).unwrap();
            assert_eq!(
                requirement_info(value.value()).unwrap(),
                ProviderRequirement {
                    id: id.into(),
                    display_name: name.into()
                }
            );
        }
        // The synthetic requirement key must NOT broaden default return/lookup
        // dispatch into the custom-provider path.
        assert!(provider_identity(first.get("Default").unwrap().value()).is_none());
        assert!(requirement_info(first.get("x").unwrap().value()).is_err());
        drop(first);
        assert_eq!(requirement_info(key.value()).unwrap(), expected);
        Module::with_temp_heap(|module| {
            let context = context("//defs:providers.bzl");
            for source in [
                "provider()",
                "DefaultInfo()",
                "platform_common.TemplateVariableInfo({})",
                "len",
                "set",
                "'PackageSpecificationInfo'",
                "None",
                "struct()",
                "42",
            ] {
                let ast =
                    AstModule::parse("invalid.bzl", source.into(), &Dialect::Standard).unwrap();
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                let value = eval.eval_module(ast, &test_globals()).unwrap();
                assert!(requirement_info(value).is_err(), "{source}");
            }
            let huge = module.heap().alloc(ProviderConstructor {
                extension: "x".repeat(8 * 1024 * 1024),
                exported: OnceLock::from("Name".to_owned()),
                fields: None,
                _doc: None,
                init: Value::new_none(),
            });
            assert!(
                requirement_info(huge)
                    .unwrap_err()
                    .to_string()
                    .contains("byte limit")
            );
        });
    }

    #[test]
    fn package_specification_custom_return_is_an_error_and_nested_payload_is_file_free() {
        let definitions = extension("//defs:providers.bzl", "P = provider()").unwrap();
        Module::with_temp_heap(|module| {
            module.import_public_symbols(&definitions);
            let package = crate::label::PackageId {
                repository: String::new(),
                package: "groups".into(),
            };
            let contents = crate::package_specification::parse_specs(
                &["//allowed".into()],
                &package,
                &crate::repository::RepositorySet::new(".".into()),
            )
            .unwrap();
            let node = std::sync::Arc::new(
                crate::package_specification::GroupNode::new(std::sync::Arc::new(contents), vec![])
                    .unwrap(),
            );
            let info = crate::package_specification::alloc_info(&module.heap(), node);
            assert!(requirement_info(info).is_err());
            module.set("info", info);
            for source in ["info", "[info]", "(info,)", "[DefaultInfo(), info]"] {
                let ast =
                    AstModule::parse("return.star", source.into(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                let error = returned_custom_providers(result).unwrap_err().to_string();
                assert!(
                    error.contains("PackageSpecificationInfo") && error.contains("unsupported"),
                    "{error}"
                );
            }
            for source in [
                "P(value=info)",
                "P(value=[{'k': struct(nested=(info,))}])",
                "P(value={info: 'key'})",
                "P(value=set([info]))",
                "P(value=[set([(info,)])])",
            ] {
                let ast =
                    AstModule::parse("return.star", source.into(), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap();
                // Forwarded inside another provider (rules_cc's CcToolchainInfo
                // `_allowlist_for_layering_check`): an owned value it may hold.
                check_freezable_payloads([result]).unwrap();
            }
        });
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
        assert_eq!(file_field(nested.value(), "file"), Some(file()));
        assert_eq!(file_field(empty.value(), "file"), None);
        assert_ne!(
            provider_identity(definitions.get("P").unwrap().value()).as_deref(),
            Some(crate::template_variable_info::PROVIDER_ID)
        );
    }

    #[test]
    fn set_payloads_keep_first_encounter_order_through_freeze_and_reowning() {
        let generated = File {
            is_directory: false,
            is_symlink: false,
            path: "bazel-rs-out/bin/pkg/generated.txt".into(),
            short_path: "pkg/generated.txt".into(),
            owner: "//pkg:producer".into(),
            is_source: false,
        };
        let second_source = File {
            is_directory: false,
            is_symlink: false,
            path: "pkg/second.txt".into(),
            short_path: "pkg/second.txt".into(),
            owner: "//pkg:second.txt".into(),
            is_source: true,
        };
        // `shared` (field `a`): first-encounter order, duplicates dropped.
        let expected = vec![generated.clone(), file()];
        let set_files = |value: Value<'_>| -> Vec<File> {
            let (_, _, fields, _) = instance_parts(value).unwrap();
            let (_, set) = fields.into_iter().find(|(name, _)| name == "a").unwrap();
            SetRef::unpack_value_opt(set)
                .unwrap()
                .iter()
                .map(|element| File::from_value(element).unwrap().clone())
                .collect()
        };
        let frozen = Module::with_temp_heap(|module| {
            module.set("file", alloc_file(module.heap(), file()));
            module.set("generated", alloc_file(module.heap(), generated));
            module.set("second", alloc_file(module.heap(), second_source));
            let context = context("//defs:sets.bzl");
            let ast = AstModule::parse("sets.bzl", r#"
P = provider()
shared = set([generated, file, generated])
nested = set([(file, P(files=(second, generated)))])
metadata = set([RunEnvironmentInfo({'MODE': 'test'}, ['HOME']), platform_common.TemplateVariableInfo({'FLAG': 'value'})])
x = P(a=shared, b=(nested, shared), c=metadata)
empty = P(files=set(), metadata=metadata)
"#.into(), &Dialect::Standard).unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast, &test_globals()).unwrap();
            }
            assert_eq!(set_files(module.get("x").unwrap()), expected);
            module.freeze().unwrap()
        });
        let payload = frozen.get("x").unwrap();
        drop(frozen);
        assert_eq!(set_files(payload.value()), expected);
        let reowned = Module::with_temp_heap(|module| {
            module.set("x", module.heap().access_owned_frozen_value(&payload));
            module.freeze().unwrap()
        });
        drop(payload);
        assert_eq!(set_files(reowned.get("x").unwrap().value()), expected);
    }

    #[test]
    fn set_constructor_is_not_a_target_provider_key() {
        let definitions = extension(
            "//defs:sets.bzl",
            "P = provider()\nx = P(files=set([file]))\nSet = set",
        )
        .unwrap();
        let payload = definitions.get("x").unwrap();
        let key = definitions.get("Set").unwrap();
        assert!(provider_identity(key.value()).is_none());
        let error = requirement_info(key.value()).unwrap_err().to_string();
        assert!(error.contains("Provider constructors"), "{error}");
        Module::with_temp_heap(|module| {
            module.import_public_symbols(&definitions);
            module.set(
                "target",
                alloc_target_with_providers(
                    module.heap(),
                    "//pkg:target",
                    vec![],
                    BTreeMap::from([(instance_identity(payload.value()).unwrap(), payload)]),
                ),
            );
            let ast = AstModule::parse("use.star", "(P in target and DefaultInfo in target) or fail('provider membership')\nlist(target[P].files) == [file] or fail('provider files')".into(), &Dialect::Standard).unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &test_globals())
                .unwrap();
            for source in [
                "set in target",
                "Set in target",
                "set not in target",
                "Set not in target",
            ] {
                let ast = AstModule::parse("use.star", source.into(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("Target membership requires an exported provider constructor"),
                    "{source}: {error}"
                );
            }
            for source in ["target[set]", "target[Set]"] {
                let ast = AstModule::parse("use.star", source.into(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("exported provider constructor"),
                    "{source}: {error}"
                );
            }
        });
    }

    #[test]
    fn target_membership_rejects_unexported_constructor_but_not_absent_exported_key() {
        let definitions = extension(
            "//defs:providers.bzl",
            "def factory():\n    return provider()\nBOX = [factory()]\nP = provider()",
        )
        .unwrap();
        Module::with_temp_heap(|module| {
            module.import_public_symbols(&definitions);
            module.set(
                "target",
                alloc_target_with_providers(module.heap(), "//pkg:target", vec![], BTreeMap::new()),
            );
            let ast = AstModule::parse("use.star", "(P not in target and DefaultInfo in target and target[DefaultInfo].files.to_list() == []) or fail('valid absent keys')\nBOX[0]".into(), &Dialect::Standard).unwrap();
            let unexported = Evaluator::new(&module)
                .eval_module(ast, &test_globals())
                .unwrap();
            assert!(ProviderConstructor::from_value(unexported).is_some());
            assert!(provider_identity(unexported).is_none());
            assert!(
                requirement_info(unexported)
                    .unwrap_err()
                    .to_string()
                    .contains("exported name")
            );
            for (source, expected) in [
                (
                    "BOX[0] in target",
                    "Target membership requires an exported provider constructor",
                ),
                (
                    "BOX[0] not in target",
                    "Target membership requires an exported provider constructor",
                ),
                (
                    "target[BOX[0]]",
                    "Target indexing requires an exported provider constructor",
                ),
                ("target[P]", "does not provide"),
                (
                    "P() in target",
                    "Target membership requires an exported provider constructor",
                ),
            ] {
                let ast = AstModule::parse("use.star", source.into(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &test_globals())
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(expected), "{source}: {error}");
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
        // The re-owned instance keeps its payload alive: `files` is [file].
        let x = reowned.get("x").unwrap();
        let (_, _, fields, _) = instance_parts(x.value()).unwrap();
        let (_, files) = fields
            .into_iter()
            .find(|(name, _)| name == "files")
            .unwrap();
        let files = ListRef::from_value(files).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(File::from_value(files[0]), Some(&file()));
    }
}
