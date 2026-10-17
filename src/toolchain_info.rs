//! Starlark side of toolchains (increment N17b): `platform_common.ToolchainInfo`
//! instances and `ctx.toolchains` (docs/TOOLCHAINS.md).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
use allocative::Allocative;
use anyhow::anyhow;
use starlark::collections::StarlarkHasher;
use starlark::environment::{Methods, MethodsBuilder};
use starlark::eval::{Arguments, Evaluator};
use starlark::values::{
    Coerce, Freeze, Heap, NoSerialize, OwnedFrozenValue, ProvidesStaticType, StarlarkPagable,
    StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLike, starlark_value,
};
use starlark::{starlark_complex_value, starlark_module, starlark_simple_value};
use std::collections::BTreeMap;
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

use crate::label::Label;
use crate::repository::RepositorySet;

/// BuiltinProvider.requirement_id of `platform_common.ToolchainInfo`
/// (crate::language_modules).
pub const PROVIDER_ID: &str = "builtin:ToolchainInfo";

// Only derive-generated marker/trace/coercion implementations need this lint
// exception.
#[allow(unsafe_code)]
mod values {
    use super::*;

    /// ToolchainInfo (analysis/platform/ToolchainInfo.java:41-106): a
    /// StructImpl whose fields are the constructor's keyword arguments,
    /// sorted by name (ImmutableSortedMap, :82-89).
    #[derive(
        Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, Freeze, Coerce, StarlarkPagable,
    )]
    #[repr(C)]
    pub(crate) struct ToolchainInfoGen<V> {
        pub fields: Vec<(String, V)>,
    }

    /// `ctx.toolchains` (analysis/starlark/StarlarkToolchainContext.java):
    /// `None` is TOOLCHAINS_NOT_VALID (:42-61), for rules without toolchain
    /// resolution.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ToolchainContext {
        #[allocative(skip)]
        pub data: Option<Arc<ToolchainContextData>>,
    }
}
pub(crate) use values::ToolchainContext;
use values::ToolchainInfoGen;

starlark_complex_value!(pub(crate) ToolchainInfo);
starlark_simple_value!(ToolchainContext);

/// ToolchainInfoApi.Provider.toolchainInfo (keyword arguments only).
pub(crate) fn construct<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    // The selfCall method has only extraKeywords
    // (starlarkbuildapi/platform/ToolchainInfoApi.java:40-50): Starlark's
    // BuiltinFunction rejects positionals (paired capture B5/starlark/ti_pos).
    if args.positions(eval.heap())?.next().is_some() {
        return Err(starlark::Error::new_other(anyhow!(
            "ToolchainInfo() got unexpected positional argument"
        )));
    }
    let mut fields = args
        .names_map()?
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), *value))
        .collect::<Vec<_>>();
    fields.sort_by(|a, b| crate::native_rules::java_string_cmp(&a.0, &b.0));
    Ok(eval.heap().alloc(ToolchainInfoGen { fields }))
}

pub(crate) fn is_instance(value: Value<'_>) -> bool {
    ToolchainInfo::from_value(value).is_some()
}

/// The field values of a ToolchainInfo (for provider file collection).
pub(crate) fn fields<'v>(value: Value<'v>) -> Option<Vec<Value<'v>>> {
    Some(
        ToolchainInfo::from_value(value)?
            .fields
            .iter()
            .map(|(_, value)| *value)
            .collect(),
    )
}

impl<V> fmt::Display for ToolchainInfoGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("struct(...)")
    }
}

#[starlark_value(type = "ToolchainInfo")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ToolchainInfoGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, name: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_value())
    }

    fn dir_attr(&self) -> Vec<String> {
        self.fields.iter().map(|(name, _)| name.clone()).collect()
    }

    /// StructImpl.repr (packages/StructImpl.java:139-153).
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

    /// StructImpl.equals (StructImpl.java:94-119): same provider and fields.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        let Some(other) = ToolchainInfo::from_value(other) else {
            return Ok(false);
        };
        if self.fields.len() != other.fields.len() {
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
        PROVIDER_ID.hash(hasher);
        for (name, value) in &self.fields {
            name.hash(hasher);
            value.to_value().write_hash(hasher)?;
        }
        Ok(())
    }
}

/// The resolved toolchain context a rule implementation sees
/// (analysis/ResolvedToolchainContext.java with
/// StarlarkToolchainContext.create, StarlarkRuleContext.java:877-898).
pub struct ToolchainContextData {
    /// Rule.toString (packages/Rule.java:665-667): `KIND rule LABEL`.
    pub target_description: String,
    /// requestedToolchainTypeLabels: each requested label (and resolved
    /// type label) with its type label, in order.
    pub requested: Vec<(Label, Label)>,
    /// The ToolchainInfo of each resolved type label.
    pub toolchains: BTreeMap<Label, OwnedFrozenValue>,
    /// String keys are parsed with the repository mapping and package of
    /// the `.bzl` of the indexing expression, the innermost calling module
    /// (LabelConverter.forBzlEvaluatingThread,
    /// analysis/starlark/StarlarkToolchainContext.java:107-126,
    /// cmdline/LabelConverter.java:41-44): the vendored interpreter passes the
    /// expression's location (StarlarkValue::at_with_location,
    /// is_in_with_location; docs/STARLARK_VENDOR.md, docs/TOOLCHAINS.md).
    pub repositories: Arc<RepositorySet>,
}

/// The lazy source location of an indexing or `in` expression.
type Location<'a> = &'a dyn Fn() -> Option<starlark::codemap::FileSpan>;

/// The `.bzl` whose expression indexes `ctx.toolchains` with a string: the
/// module name of the expression's location (`//pkg:defs.bzl`,
/// `@@repo//:defs.bzl`). Without a location, or with a module name that is
/// not a label, the key fails explicitly (no guessed mapping).
fn caller_bzl(text: &str, location: Location<'_>) -> anyhow::Result<Label> {
    let span = location().ok_or_else(|| {
        anyhow!(
            "ctx.toolchains[\"{text}\"]: the native profile cannot determine the calling .bzl file of this string key (no source location), so it cannot choose its repository mapping (LabelConverter.forBzlEvaluatingThread)"
        )
    })?;
    let file = span.filename();
    Label::parse_canonical(file).map_err(|_| {
        anyhow!(
            "ctx.toolchains[\"{text}\"]: the calling file {file:?} is not a .bzl label, so the native profile cannot choose its repository mapping (LabelConverter.forBzlEvaluatingThread)"
        )
    })
}

/// LabelConverter.forBzlEvaluatingThread(thread).convert(text)
/// (cmdline/LabelConverter.java:41-44) for a label string an analysis-time
/// builtin receives (`ctx.actions.run(toolchain = "...")`): parsed with the
/// innermost calling `.bzl`'s repository mapping and package; a syntax
/// error is the LabelSyntaxException's message
/// (StarlarkActionFactory.determineExecGroup, :861-871), an apparent
/// repository the caller does not see stays unresolvable.
pub(crate) fn parse_bzl_label(
    repositories: &RepositorySet,
    text: &str,
    location: Location<'_>,
) -> anyhow::Result<Label> {
    crate::bazel_label::Parts::parse(text).map_err(|error| anyhow!("{error}"))?;
    let (repository, package) = if text.starts_with("@@") {
        (String::new(), String::new())
    } else {
        let caller = caller_bzl(text, location)?;
        (caller.repository, caller.package)
    };
    match repositories.parse(text, &repository, &package) {
        Ok(label) => Ok(label),
        Err(error) if error.starts_with("unknown apparent repository") => {
            crate::toolchains::deferred_label_text(text, &repository)
                .ok_or_else(|| anyhow!("{error}"))
                .and_then(|text| crate::toolchains::parse_label(&text).map_err(anyhow::Error::msg))
        }
        Err(error) => Err(anyhow!("{error}")),
    }
}

impl fmt::Debug for ToolchainContextData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolchainContextData")
            .field("target_description", &self.target_description)
            .field("requested", &self.requested)
            .finish_non_exhaustive()
    }
}

fn shown(label: &Label) -> String {
    crate::rule_declarations::canonical_form(label)
}

impl ToolchainContextData {
    /// StarlarkToolchainContext.transformKey (:107-126).
    fn transform_key(&self, key: Value<'_>, location: Location<'_>) -> anyhow::Result<Label> {
        if let Some(label) = crate::starlark_types::unpack_label(key) {
            return Label::parse_canonical(&label).map_err(anyhow::Error::msg);
        }
        if let Some(text) = key.unpack_str() {
            let invalid =
                |error: String| anyhow!("Unable to parse toolchain label '{text}': {error}");
            // LabelParser's syntax errors first (cmdline/LabelParser.java).
            crate::bazel_label::Parts::parse(text).map_err(invalid)?;
            // A canonical repository does not depend on the caller.
            let (repository, package) = if text.starts_with("@@") {
                (String::new(), String::new())
            } else {
                let caller = caller_bzl(text, location)?;
                (caller.repository, caller.package)
            };
            return match self.repositories.parse(text, &repository, &package) {
                Ok(label) => Ok(label),
                // Label.parseWithRepoContext keeps an apparent repository the
                // caller does not see (paired capture B5/starlark/s2_app).
                Err(error) if error.starts_with("unknown apparent repository") => {
                    crate::toolchains::deferred_label_text(text, &repository)
                        .ok_or_else(|| invalid(error))
                        .and_then(|text| {
                            crate::toolchains::parse_label(&text).map_err(anyhow::Error::msg)
                        })
                }
                Err(error) => Err(invalid(error)),
            };
        }
        Err(anyhow!(
            "Toolchains only supports indexing by toolchain type, got {} instead",
            key.get_type()
        ))
    }

    fn type_label(&self, label: &Label) -> Option<&Label> {
        self.requested
            .iter()
            .find(|(requested, _)| requested == label)
            .map(|(_, type_label)| type_label)
    }
}

impl fmt::Display for ToolchainContext {
    /// StarlarkToolchainContext.repr (:91-97).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let labels = self
            .data
            .as_ref()
            .map(|data| {
                data.requested
                    .iter()
                    .map(|(label, _)| shown(label))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        match &self.data {
            Some(_) => write!(f, "<toolchain_context.resolved_labels: {labels}>"),
            None => write!(f, "<toolchain_context.resolved_labels: >"),
        }
    }
}

starlark::methods_static!(CONTEXT_METHODS = context_methods);

#[starlark_module]
fn context_methods(builder: &mut MethodsBuilder) {
    /// ToolchainContextApi.toolchainTypes (starlarkbuildapi/platform/ToolchainContextApi.java:43-44):
    /// the requested (and resolved) type labels; none when toolchains are
    /// not valid (StarlarkToolchainContext.java:57-60).
    fn toolchain_types<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let context = this
            .downcast_ref::<ToolchainContext>()
            .ok_or_else(|| anyhow!("expected ToolchainContext"))?;
        let labels = match &context.data {
            Some(data) => data
                .requested
                .iter()
                .map(|(label, _)| crate::starlark_types::alloc_label(heap, &shown(label)))
                .collect::<anyhow::Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        Ok(heap.alloc(labels))
    }
}

#[starlark_value(type = "ToolchainContext")]
impl<'v> StarlarkValue<'v> for ToolchainContext {
    fn get_methods() -> Option<&'static Methods> {
        Some(CONTEXT_METHODS.methods())
    }

    /// Indexing outside the bytecode interpreter has no expression location:
    /// string keys fail explicitly there.
    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        self.at_with_location(index, heap, &|| None)
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        self.is_in_with_location(other, &|| None)
    }

    /// StarlarkToolchainContext.getIndex (:129-146).
    fn at_with_location(
        &self,
        index: Value<'v>,
        heap: Heap<'v>,
        location: Location<'_>,
    ) -> starlark::Result<Value<'v>> {
        let Some(data) = &self.data else {
            return Err(starlark::Error::new_other(anyhow!(
                "Toolchains are not valid in this context"
            )));
        };
        let label = data
            .transform_key(index, location)
            .map_err(starlark::Error::new_other)?;
        let Some(type_label) = data.type_label(&label) else {
            return Err(starlark::Error::new_other(anyhow!(
                "In {}, toolchain type {} was requested but only types [{}] are configured",
                data.target_description,
                shown(&label),
                data.requested
                    .iter()
                    .map(|(label, _)| shown(label))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        Ok(match data.toolchains.get(type_label) {
            Some(info) => heap.access_owned_frozen_value(info),
            None => Value::new_none(),
        })
    }

    /// StarlarkToolchainContext.containsKey (:141-147): the requested types.
    fn is_in_with_location(
        &self,
        other: Value<'v>,
        location: Location<'_>,
    ) -> starlark::Result<bool> {
        let Some(data) = &self.data else {
            return Ok(false);
        };
        let label = data
            .transform_key(other, location)
            .map_err(starlark::Error::new_other)?;
        Ok(data.type_label(&label).is_some())
    }
}

/// The ToolchainInfo values of a `ctx.toolchains` value (an escaped
/// context in a provider), for provider file collection.
pub(crate) fn context_infos<'v>(value: Value<'v>) -> Option<Vec<Value<'v>>> {
    let context = ToolchainContext::from_value(value)?;
    Some(
        context
            .data
            .iter()
            .flat_map(|data| data.toolchains.values())
            .map(|info| info.value())
            .collect(),
    )
}

pub(crate) fn alloc_context(heap: Heap<'_>, data: Option<Arc<ToolchainContextData>>) -> Value<'_> {
    heap.alloc(ToolchainContext { data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{Globals, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn data() -> Arc<ToolchainContextData> {
        Arc::new(ToolchainContextData {
            target_description: "r rule //p:t".into(),
            requested: vec![(label("//tc:type"), label("//tc:type"))],
            toolchains: BTreeMap::new(),
            repositories: Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
        })
    }

    #[test]
    fn string_keys_use_the_expression_file_or_fail_explicitly() {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let context = alloc_context(heap, Some(data()));
            // Outside the bytecode interpreter there is no location: string
            // keys fail explicitly, Labels and canonical strings work.
            let error = context
                .at(heap.alloc("//tc:type"), heap)
                .unwrap_err()
                .to_string();
            assert!(error.contains("no source location"), "{error}");
            assert!(context.is_in(heap.alloc("//tc:type")).is_err());
            let key = crate::starlark_types::alloc_label(heap, "//tc:type").unwrap();
            assert!(context.at(key, heap).unwrap().is_none());
            assert!(
                context
                    .at(heap.alloc("@@//tc:type"), heap)
                    .unwrap()
                    .is_none()
            );
            module.set("t", context);
            let run = |file: &str, code: &str| {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(
                    AstModule::parse(file, code.to_owned(), &Dialect::Standard).unwrap(),
                    &Globals::standard(),
                )
                .map(|value| value.to_string())
                .map_err(|error| error.to_string())
            };
            // The indexing expression's module names the calling .bzl.
            assert_eq!(run("//x:defs.bzl", "t['//tc:type']").unwrap(), "None");
            assert_eq!(run("//tc:defs.bzl", "':type' in t").unwrap(), "True");
            assert_eq!(run("//x:defs.bzl", "':type' in t").unwrap(), "False");
            let error = run("ctx.star", "t['//tc:type']").unwrap_err();
            assert!(
                error.contains("the calling file \"ctx.star\" is not a .bzl label"),
                "{error}"
            );
        });
    }
}
