//! Native TemplateVariableInfo: an immutable, ordered string snapshot.
//!
//! Each variables access returns a fresh mutable dict, including after freezing
//! or crossing a dependency boundary. Runtime instance identities only support
//! Starlark equality/hashing; they are never serialized into action keys.

use allocative::Allocative;
use anyhow::anyhow;
use starlark::collections::StarlarkHasher;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};

/// Disjoint from custom providers' canonical extension-label `%` export IDs.
pub const PROVIDER_ID: &str = "builtin:TemplateVariableInfo";
static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

// Only derive-generated marker implementations need the exception. These
// simple values own all their data and freeze unchanged, without heap references.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Constructor;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct TemplateVariableInfo {
        pub id: u64,
        pub variables: Vec<(String, String)>,
    }
}
use owned::{Constructor, TemplateVariableInfo};

starlark_simple_value!(Constructor);
starlark_simple_value!(TemplateVariableInfo);

/// Install Bazel's `platform_common` namespace. Only TemplateVariableInfo is
/// implemented; the other providers exist and fail explicitly when called.
pub fn install(globals: &mut GlobalsBuilder) {
    globals.set(
        "platform_common",
        crate::language_modules::platform_common(),
    );
}

pub(crate) fn alloc_constructor(heap: Heap<'_>) -> Value<'_> {
    heap.alloc(Constructor)
}

pub fn is_constructor(value: Value<'_>) -> bool {
    Constructor::from_value(value).is_some()
}

pub fn is_instance(value: Value<'_>) -> bool {
    TemplateVariableInfo::from_value(value).is_some()
}

impl fmt::Display for Constructor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<function TemplateVariableInfo>")
    }
}

#[starlark_value(type = "Provider")]
impl<'v> StarlarkValue<'v> for Constructor {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let mut positional = args.positions(eval.heap())?;
        let mut vars = positional.next();
        if positional.next().is_some() {
            return Err(starlark::Error::new_other(anyhow!(
                "TemplateVariableInfo: expected exactly one argument, vars"
            )));
        }
        for (name, value) in args.names_map()?.iter() {
            if name.as_str() != "vars" {
                return Err(starlark::Error::new_other(anyhow!(
                    "TemplateVariableInfo: unknown argument {}",
                    name.as_str()
                )));
            }
            if vars.replace(*value).is_some() {
                return Err(starlark::Error::new_other(anyhow!(
                    "TemplateVariableInfo: duplicate argument vars"
                )));
            }
        }
        let vars = vars.ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "TemplateVariableInfo: missing required argument vars"
            ))
        })?;
        let dict = DictRef::from_value(vars).ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "TemplateVariableInfo: vars must be a dict of strings to strings"
            ))
        })?;
        let mut variables = Vec::with_capacity(dict.len());
        for (key, value) in dict.iter() {
            let (Some(key), Some(value)) = (key.unpack_str(), value.unpack_str()) else {
                return Err(starlark::Error::new_other(anyhow!(
                    "TemplateVariableInfo: vars keys and values must be strings"
                )));
            };
            variables.push((key.to_owned(), value.to_owned()));
        }
        Ok(eval.heap().alloc(TemplateVariableInfo {
            id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            variables,
        }))
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(is_constructor(other))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        Ok(())
    }
}

impl fmt::Display for TemplateVariableInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TemplateVariableInfo(...)")
    }
}

#[starlark_value(type = "TemplateVariableInfo")]
impl<'v> StarlarkValue<'v> for TemplateVariableInfo {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        (name == "variables").then(|| {
            heap.alloc(AllocDict(
                self.variables
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            ))
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["variables".to_owned()]
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some_and(|other| self.id == other.id))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        self.id.hash(hasher);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{instance_identity, provider_identity};
    use crate::starlark_types::{alloc_target_with_providers, analysis_globals};
    use starlark::environment::{FrozenModule, Globals, LibraryExtension, Module};
    use starlark::syntax::{AstModule, Dialect};
    use std::collections::BTreeMap;

    fn globals() -> Globals {
        GlobalsBuilder::extended_by(&[LibraryExtension::StructType])
            .with(analysis_globals)
            .with(install)
            .build()
    }

    fn evaluate(source: &str) -> anyhow::Result<FrozenModule> {
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse("template.bzl", source.to_owned(), &Dialect::Standard)
                .map_err(|error| anyhow!("{error}"))?;
            Evaluator::new(&module)
                .eval_module(ast, &globals())
                .map_err(|error| anyhow!("{error}"))?;
            module.freeze().map_err(|error| anyhow!("{error:?}"))
        })
    }

    fn pass(source: &str) {
        evaluate(source).unwrap();
    }

    #[test]
    fn template_variable_constructor_arguments_and_namespace() {
        pass(
            r#"
T = platform_common.TemplateVariableInfo
(type(T) == "Provider" and type(platform_common) == "platform_common") or fail("namespace")
(dir(platform_common) == ["ConstraintSettingInfo", "ConstraintValueInfo", "PlatformInfo", "TemplateVariableInfo", "ToolchainInfo"]) or fail("dir")
(T == platform_common.TemplateVariableInfo and {T: 1}[platform_common.TemplateVariableInfo] == 1) or fail("key identity")
(T({}).variables == {} and T(vars={"x": "y"}).variables == {"x": "y"}) or fail("arguments")
(T(*[{"x": "y"}]).variables == T(**{"vars": {"x": "y"}}).variables) or fail("expanded arguments")
(type(T({})) == "TemplateVariableInfo" and type(T({}).variables) == "dict") or fail("types")
(dir(T({})) == ["variables"] and not hasattr(T({}), "unknown")) or fail("fields")
"#,
        );
        for source in [
            "T()",
            "T(None)",
            "T([])",
            "T('x')",
            "T({1: 'x'})",
            "T({'x': 1})",
            "T({'x': None})",
            "T({'x': []})",
            "T({}, {})",
            "T({}, vars={})",
            "T(variables={})",
            "T(vars={}, unknown={})",
            "T(vars={}, **{'vars': {}})",
            "T(**{1: {}})",
            "T({}).variables = {}",
            "T({}).unknown",
            // Present (Bazel's namespace) but analysis-time only in the native profile.
            "platform_common.ToolchainInfo()",
            "platform_common.ConstraintValueInfo()",
            "platform_common.Unknown",
        ] {
            assert!(
                evaluate(&format!(
                    "T = platform_common.TemplateVariableInfo\n{source}"
                ))
                .is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn template_variable_snapshot_order_fresh_mutable_dict_and_identity() {
        pass(
            r#"
T = platform_common.TemplateVariableInfo
original = {"z": "first", "a": "second", "m": "third"}
x = T(original)
y = T(original)
original["z"] = "caller-mutated"
original["new"] = "late"
first = x.variables
first["z"] = "first-mutated"
first.pop("a")
second = x.variables
second["m"] = "second-mutated"
(x.variables == {"z": "first", "a": "second", "m": "third"}) or fail("snapshot")
(list(x.variables) == ["z", "a", "m"]) or fail("insertion order")
(first["z"] == "first-mutated" and second["z"] == "first") or fail("independent reads")
(x == x and x != y and x.variables == y.variables) or fail("instance identity")
keys = {x: "x", y: "y"}
(len(keys) == 2 and keys[x] == "x" and keys[y] == "y") or fail("hash identity")
(x != T and x != struct(variables=x.variables)) or fail("native identity")
"#,
        );
    }

    #[test]
    fn template_variable_frozen_clones_and_reexported_constructor_identity() {
        let first = evaluate("T = platform_common.TemplateVariableInfo\nx = T({'FLAG': 'seed'})\nAlias = T\nnested = [T]").unwrap();
        let second = evaluate("Other = platform_common.TemplateVariableInfo").unwrap();
        let constructor = first.get("T").unwrap();
        let alias = first.get("Alias").unwrap();
        let other = second.get("Other").unwrap();
        let instance = first.get("x").unwrap();
        let instance_clone = instance.clone();
        assert_eq!(
            provider_identity(constructor.value()).as_deref(),
            Some(PROVIDER_ID)
        );
        assert_eq!(
            provider_identity(alias.value()),
            provider_identity(other.value())
        );
        assert_eq!(
            instance_identity(instance.value()).as_deref(),
            Some(PROVIDER_ID)
        );
        assert!(provider_identity(instance.value()).is_none());
        assert!(instance_identity(constructor.value()).is_none());
        let reexported = Module::with_temp_heap(|module| {
            module.import_public_symbols(&first);
            let ast = AstModule::parse(
                "alias.bzl",
                "Reexport = nested[0]".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &globals())
                .unwrap();
            module.freeze().unwrap()
        });
        let reexport = reexported.get("Reexport").unwrap();
        drop(first);
        drop(second);
        drop(reexported);
        let consumer = Module::with_temp_heap(|module| {
            module.set("T", module.heap().access_owned_frozen_value(&other));
            module.set(
                "Reexport",
                module.heap().access_owned_frozen_value(&reexport),
            );
            module.set(
                "same",
                module.heap().access_owned_frozen_value(&instance_clone),
            );
            let cloned = TemplateVariableInfo::from_value(instance.value())
                .unwrap()
                .clone();
            module.set("rust_clone", module.heap().alloc(cloned));
            module.set(
                "target",
                alloc_target_with_providers(
                    module.heap(),
                    "//pkg:target",
                    vec![],
                    BTreeMap::from([(PROVIDER_ID.to_owned(), instance)]),
                ),
            );
            let ast = AstModule::parse(
                "consumer.bzl",
                r#"
(T == Reexport and {T: 1}[Reexport] == 1) or fail("constructor identity")
(T in target and Reexport in target and DefaultInfo in target) or fail("membership")
x = target[Reexport]
(x == same and x == rust_clone and {same: 1}[rust_clone] == 1) or fail("preserved identity")
copy = x.variables
copy["FLAG"] = "consumer-mutated"
(x.variables == {"FLAG": "seed"}) or fail("consumer snapshot")
(target[T] == x and target[T].variables["FLAG"] == "seed") or fail("lookup")
"#
                .to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &globals())
                .unwrap();
            for source in [
                "T({}) not in target",
                "'builtin:TemplateVariableInfo' not in target",
            ] {
                let ast =
                    AstModule::parse("invalid.bzl", source.into(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &globals())
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("Target membership requires an exported provider constructor"),
                    "{source}: {error}"
                );
            }
            let ast = AstModule::parse(
                "invalid.bzl",
                "target[T({})]".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            assert!(
                Evaluator::new(&module)
                    .eval_module(ast, &globals())
                    .is_err()
            );
            module.freeze().unwrap()
        });
        drop(instance_clone);
        drop(other);
        drop(reexport);
        let value = consumer.get("x").unwrap();
        assert_eq!(
            TemplateVariableInfo::from_value(value.value())
                .unwrap()
                .variables,
            vec![("FLAG".to_owned(), "seed".to_owned())]
        );
        assert!(
            value
                .value()
                .equals(consumer.get("rust_clone").unwrap().value())
                .unwrap()
        );
    }
}
