//! `testing.ExecutionInfo` (Bazel 9.3 analysis/test/ExecutionInfo.java,
//! starlarkbuildapi/test/ExecutionInfoApi.java): a NativeInfo with the struct
//! fields `requirements` (a string-to-string dict) and `exec_group` (a
//! string, default `"test"`).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (Release 9.3.0); paths
//! are relative to `src/main/java/com/google/devtools/build/lib/`. A rule
//! returns it as a provider and others read it (`dep[testing.ExecutionInfo]`,
//! rules_python's py_interpreter_program and precompile.bzl). Bazel itself
//! reads it only for test rules (RuleConfiguredTargetBuilder.initializeTestProvider):
//! the requirements join the test action's execution info and the exec group
//! chooses the test action's owner ([`TestRequirements`], recorded by
//! crate::analysis, applied by the `test` command). Paired capture
//! tests/fixtures/execution_info.

use allocative::Allocative;
use anyhow::anyhow;
use starlark::collections::StarlarkHasher;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

/// BuiltinProvider.requirement_id of `testing.ExecutionInfo`
/// (crate::language_modules).
pub(crate) const PROVIDER_ID: &str = "builtin:ExecutionInfo";

/// The selfCall method name used in binding errors.
const NAME: &str = "ExecutionInfo";

/// The parameters in order, with their `want` type spelling
/// (ExecutionInfoApi.java:62-79).
const PARAMS: [(&str, &str); 2] = [("requirements", "dict"), ("exec_group", "string")];

/// RuleClass.DEFAULT_TEST_RUNNER_EXEC_GROUP_NAME (the `'test'` default).
const DEFAULT_EXEC_GROUP: &str = "test";

#[derive(Debug, Allocative)]
struct Data {
    /// ImmutableMap.copyOf(requirements): the dict's insertion order.
    requirements: Vec<(String, String)>,
    /// `requirements` indices sorted by key: map equality and hashing ignore
    /// the order (keys are unique).
    canonical: Vec<usize>,
    exec_group: String,
}

// Only derive-generated marker implementations need the exception. The value
// owns plain strings and freezes unchanged, without heap references.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct ExecutionInfo(pub(super) Arc<Data>);
}
use owned::ExecutionInfo;

starlark_simple_value!(ExecutionInfo);

fn failure(message: String) -> starlark::Error {
    starlark::Error::new_other(anyhow!(message))
}

pub(crate) fn is_instance(value: Value<'_>) -> bool {
    ExecutionInfo::from_value(value).is_some()
}

/// A test rule's returned ExecutionInfo as its test action reads it
/// (RuleConfiguredTargetBuilder.initializeTestProvider :470-498, analysis/
/// RuleConfiguredTargetBuilder.java, `setExecutionRequirements`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TestRequirements {
    /// ExecutionInfo.getExecutionInfo in insertion order: put over the test
    /// action's execution info, replacing the values of keys the rule's tags
    /// set (TestTargetProperties.java:115-118).
    pub requirements: Vec<(String, String)>,
    /// ExecutionInfo.getExecGroup: the exec group whose action owner runs
    /// the test (TestActionBuilder.getTestActionOwner, analysis/test/
    /// TestActionBuilder.java:151-170).
    pub exec_group: String,
}

impl TestRequirements {
    /// Whether the test action keeps the default test runner exec group
    /// (RuleClass.DEFAULT_TEST_RUNNER_EXEC_GROUP_NAME): its owner is the
    /// `test` group's, the owner of a test without ExecutionInfo too (without
    /// --use_target_platform_for_tests, whose test action owner has no
    /// native counterpart, docs/TEST_FLAGS.md).
    pub fn default_exec_group(&self) -> bool {
        self.exec_group == DEFAULT_EXEC_GROUP
    }
}

/// The requirements and exec group of an ExecutionInfo instance.
pub(crate) fn test_requirements(value: Value<'_>) -> Option<TestRequirements> {
    let info = ExecutionInfo::from_value(value)?;
    Some(TestRequirements {
        requirements: info.0.requirements.clone(),
        exec_group: info.0.exec_group.clone(),
    })
}

/// ExecutionInfoProvider.constructor (analysis/test/ExecutionInfo.java:75-86)
/// with Java Starlark's BuiltinFunction binding
/// (net/starlark/java/eval/BuiltinFunction.java:308-445): positional
/// arguments, then named ones, are type-checked as they are bound (a named
/// argument's type before a duplicate), surplus positional arguments are
/// reported after all of them, then `Dict.cast(requirements, String, String,
/// "requirements")` (net/starlark/java/eval/Dict.java:659-684).
pub(crate) fn construct<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    let check = |index: usize, value: Value<'v>| -> starlark::Result<Value<'v>> {
        let (param, want) = PARAMS[index];
        let ok = match index {
            0 => DictRef::from_value(value).is_some(),
            _ => value.unpack_str().is_some(),
        };
        if ok {
            Ok(value)
        } else {
            Err(failure(format!(
                "in call to {NAME}(), parameter '{param}' got value of type '{}', want '{want}'",
                value.get_type()
            )))
        }
    };
    let mut bound: [Option<Value<'v>>; 2] = [None, None];
    let positional: Vec<Value<'v>> = args.positions(heap)?.collect();
    for (index, value) in positional.iter().take(PARAMS.len()).enumerate() {
        bound[index] = Some(check(index, *value)?);
    }
    let names: Vec<&str> = PARAMS.iter().map(|(name, _)| *name).collect();
    for (name, value) in args.names_map()?.iter() {
        let name = name.as_str();
        let Some(index) = names.iter().position(|known| *known == name) else {
            return Err(failure(format!(
                "{NAME}() got unexpected keyword argument '{name}'{}",
                crate::native_rules::did_you_mean(name, &names)
            )));
        };
        let value = check(index, *value)?;
        if bound[index].replace(value).is_some() {
            return Err(failure(format!(
                "{NAME}() got multiple values for argument '{name}'"
            )));
        }
    }
    if positional.len() > PARAMS.len() {
        return Err(failure(format!(
            "{NAME}() accepts no more than {} positional arguments but got {}",
            PARAMS.len(),
            positional.len()
        )));
    }
    let mut requirements = Vec::new();
    if let Some(dict) = bound[0].and_then(DictRef::from_value) {
        requirements.reserve(dict.len());
        for (key, value) in dict.iter() {
            let (Some(text_key), Some(text_value)) = (key.unpack_str(), value.unpack_str()) else {
                return Err(failure(format!(
                    "got dict<{}, {}> for 'requirements', want dict<string, string>",
                    key.get_type(),
                    value.get_type()
                )));
            };
            requirements.push((text_key.to_owned(), text_value.to_owned()));
        }
    }
    let exec_group = bound[1]
        .and_then(|value| value.unpack_str())
        .unwrap_or(DEFAULT_EXEC_GROUP)
        .to_owned();
    let mut canonical: Vec<usize> = (0..requirements.len()).collect();
    canonical.sort_unstable_by(|&a, &b| requirements[a].0.cmp(&requirements[b].0));
    Ok(heap.alloc(ExecutionInfo(Arc::new(Data {
        requirements,
        canonical,
        exec_group,
    }))))
}

/// NativeInfo.repr (packages/NativeInfo.java:120-136): the sorted struct
/// fields, values in Bazel's Printer quoting.
impl fmt::Display for ExecutionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("struct(exec_group = ")?;
        crate::run_environment_info::write_quoted(f, &self.0.exec_group)?;
        f.write_str(", requirements = {")?;
        for (index, (key, value)) in self.0.requirements.iter().enumerate() {
            if index != 0 {
                f.write_str(", ")?;
            }
            crate::run_environment_info::write_quoted(f, key)?;
            f.write_str(": ")?;
            crate::run_environment_info::write_quoted(f, value)?;
        }
        f.write_str("})")
    }
}

#[starlark_value(type = "ExecutionInfo")]
impl<'v> StarlarkValue<'v> for ExecutionInfo {
    /// The structField getters (ExecutionInfoApi.java:40-50); the map is a
    /// fresh mutable dict on the caller's heap (Starlark.fromJava with the
    /// thread's mutability).
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match name {
            "requirements" => Some(
                heap.alloc(AllocDict(
                    self.0
                        .requirements
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str())),
                )),
            ),
            "exec_group" => Some(heap.alloc(self.0.exec_group.as_str())),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["exec_group".to_owned(), "requirements".to_owned()]
    }

    /// EvalUtils.setField on a non-Structure value.
    fn set_attr(&self, attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
        Err(failure(format!(
            "cannot set .{attribute} field of {NAME} value"
        )))
    }

    /// NativeInfo.equals (:96-112): the same provider and equal fields (map
    /// equality ignores the order).
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(ExecutionInfo::from_value(other).is_some_and(|other| {
            self.0.exec_group == other.0.exec_group
                && self.0.requirements.len() == other.0.requirements.len()
                && self
                    .0
                    .canonical
                    .iter()
                    .zip(&other.0.canonical)
                    .all(|(&a, &b)| self.0.requirements[a] == other.0.requirements[b])
        }))
    }

    /// NativeInfo is immutable and hashable (:58-61); consistent with
    /// `equals`.
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        self.0.exec_group.hash(hasher);
        self.0.requirements.len().hash(hasher);
        for &index in &self.0.canonical {
            self.0.requirements[index].hash(hasher);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use starlark::environment::Module;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    fn eval(source: &str) -> Result<(), String> {
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse(
                "execution_info.bzl",
                format!("E = testing.ExecutionInfo\n{source}"),
                &Dialect::Standard,
            )
            .map_err(|error| error.to_string())?;
            Evaluator::new(&module)
                .eval_module(ast, &crate::package::bzl_globals())
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    /// The genuine 9.3.0 surface (tests/fixtures/execution_info case `probe`).
    #[test]
    fn execution_info_binds_like_bazel_and_compares_by_content() {
        eval(
            r#"
a = E({"requires-x": "1", "b": ""})
(type(E) == "Provider" and type(a) == "ExecutionInfo" and dir(a) == ["exec_group", "requirements"]) or fail("types")
(repr(a) == 'struct(exec_group = "test", requirements = {"requires-x": "1", "b": ""})' and str(a) == repr(a)) or fail(repr(a))
(a.exec_group == "test" and a.requirements == {"requires-x": "1", "b": ""}) or fail("fields")
p = a.requirements
p["x"] = "y"
(a.requirements == {"requires-x": "1", "b": ""}) or fail("fresh projection")
(repr(E()) == 'struct(exec_group = "test", requirements = {})') or fail("defaults")
(E({"k": "v"}, "g") == E(exec_group = "g", requirements = {"k": "v"})) or fail("binding")
(E({"a": "1", "b": "2"}) == E({"b": "2", "a": "1"}) and E({"k": "v"}) != E({"k": "v"}, "g")) or fail("equality")
(E() != struct(exec_group = "test", requirements = {})) or fail("struct")
({a: 1}.get(E({"b": "", "requires-x": "1"})) == 1) or fail("hash")
(not hasattr(a, "nope") and getattr(a, "nope", 3) == 3) or fail("missing")
"#,
        )
        .unwrap();
        for (source, message) in [
            (
                "E([])",
                "in call to ExecutionInfo(), parameter 'requirements' got value of type 'list', want 'dict'",
            ),
            (
                "E(exec_group = None)",
                "in call to ExecutionInfo(), parameter 'exec_group' got value of type 'NoneType', want 'string'",
            ),
            (
                "E({1: 'a'})",
                "got dict<int, string> for 'requirements', want dict<string, string>",
            ),
            (
                "E({'a': 1})",
                "got dict<string, int> for 'requirements', want dict<string, string>",
            ),
            (
                "E(requirement = {})",
                "ExecutionInfo() got unexpected keyword argument 'requirement' (did you mean 'requirements'?)",
            ),
            (
                "E({}, 'a', 'b')",
                "ExecutionInfo() accepts no more than 2 positional arguments but got 3",
            ),
            // Positional types are checked while binding, before the count.
            (
                "E([], 'a', 'b')",
                "in call to ExecutionInfo(), parameter 'requirements' got value of type 'list', want 'dict'",
            ),
            (
                "E({}, requirements = {})",
                "ExecutionInfo() got multiple values for argument 'requirements'",
            ),
            // A named argument's type is checked before the duplicate.
            (
                "E({}, requirements = [])",
                "in call to ExecutionInfo(), parameter 'requirements' got value of type 'list', want 'dict'",
            ),
            (
                "x = E()\nx.requirements = {}",
                "cannot set .requirements field of ExecutionInfo value",
            ),
        ] {
            let error = eval(source).unwrap_err();
            assert!(error.contains(message), "{source}: {error}");
        }
    }
}
