//! Owned RunEnvironmentInfo metadata, not an implicit action/run environment.
//!
//! Inputs are copied without freezing them. Each field access projects a fresh
//! mutable value on the caller's heap, even when the receiver is frozen.

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::collections::StarlarkHasher;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::AllocList;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

pub(crate) const PROVIDER_ID: &str = "builtin:RunEnvironmentInfo";

#[derive(Debug, Allocative)]
struct InfoData {
    environment: Vec<(String, String)>,
    inherited_environment: Vec<String>,
    // Dict keys are unique. Sort only this index, preserving insertion order in
    // projections and enabling allocation-free linear equality and hashing.
    canonical: Vec<usize>,
    // RunEnvironmentInfo.shouldErrorOnNonExecutableRule: true for the public
    // constructor, false for testing.TestEnvironment. Not part of equality
    // (Bazel's NativeInfo equality compares provider fields only).
    error_on_non_executable: bool,
}

// Only derive-generated markers require this exception. No owned value contains
// a Starlark heap reference; simple-value freezing keeps the owned snapshot.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Constructor;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct RunEnvironmentInfo(pub(super) Arc<InfoData>);
}
use owned::{Constructor, RunEnvironmentInfo};

starlark_simple_value!(Constructor);
starlark_simple_value!(RunEnvironmentInfo);

pub(crate) fn install(builder: &mut GlobalsBuilder) {
    builder.set("RunEnvironmentInfo", Constructor);
}

/// `testing.TestEnvironment(environment, inherited_environment)`:
/// StarlarkTestingModule.testEnvironment builds a RunEnvironmentInfo with
/// shouldErrorOnNonExecutableRule=false.
pub(crate) fn alloc_test_environment<'v>(
    heap: Heap<'v>,
    environment: Value<'v>,
    inherited: Option<Value<'v>>,
) -> anyhow::Result<Value<'v>> {
    let data = snapshot(Some(environment), inherited, false)?;
    Ok(heap.alloc(RunEnvironmentInfo(Arc::new(data))))
}

/// Whether returning this instance from a non-executable, non-test rule is
/// an error (public constructor) rather than a warning (TestEnvironment).
pub(crate) fn errors_on_non_executable_rule(value: Value<'_>) -> bool {
    RunEnvironmentInfo::from_value(value).is_none_or(|info| info.0.error_on_non_executable)
}

pub(crate) fn is_constructor(value: Value<'_>) -> bool {
    Constructor::from_value(value).is_some()
}

pub(crate) fn is_instance(value: Value<'_>) -> bool {
    RunEnvironmentInfo::from_value(value).is_some()
}

impl fmt::Display for Constructor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<function RunEnvironmentInfo>")
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
        let mut environment = positional.next();
        let mut inherited = positional.next();
        if positional.next().is_some() {
            return Err(starlark::Error::new_other(anyhow!(
                "RunEnvironmentInfo: expected at most two positional arguments"
            )));
        }
        for (name, value) in args.names_map()?.iter() {
            let slot = match name.as_str() {
                "environment" => &mut environment,
                "inherited_environment" => &mut inherited,
                _ => {
                    return Err(starlark::Error::new_other(anyhow!(
                        "RunEnvironmentInfo: unknown argument {}",
                        name.as_str()
                    )));
                }
            };
            if slot.replace(*value).is_some() {
                return Err(starlark::Error::new_other(anyhow!(
                    "RunEnvironmentInfo: duplicate argument {}",
                    name.as_str()
                )));
            }
        }
        let data = snapshot(environment, inherited, true).map_err(starlark::Error::new_other)?;
        Ok(eval.heap().alloc(RunEnvironmentInfo(Arc::new(data))))
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(is_constructor(other))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        Ok(())
    }
}

fn snapshot(
    environment: Option<Value<'_>>,
    inherited: Option<Value<'_>>,
    error_on_non_executable: bool,
) -> anyhow::Result<InfoData> {
    let environment = environment
        .map(|value| {
            DictRef::from_value(value).ok_or_else(|| {
                anyhow!("RunEnvironmentInfo: environment must be a dict of strings to strings")
            })
        })
        .transpose()?;
    let inherited = match inherited {
        None => &[],
        Some(value) => crate::custom_rule::sequence_ref(value).ok_or_else(|| {
            anyhow!("RunEnvironmentInfo: inherited_environment must be a list or tuple of strings")
        })?,
    };

    // Preflight BOTH borrowed fields before any payload clone/reservation. The
    // existing weighted-copy profile reserves projection/escaped repr space as
    // well as retained strings and container nodes. Getters cannot report budget
    // failures, so their individual expansion is bounded here at construction.
    let mut budget = crate::repository_budget::HostBudget::default();
    budget.node().map_err(anyhow::Error::msg)?;
    budget.node().map_err(anyhow::Error::msg)?;
    budget.node().map_err(anyhow::Error::msg)?;
    if let Some(dict) = &environment {
        for (key, value) in dict.iter() {
            let (Some(key), Some(value)) = (key.unpack_str(), value.unpack_str()) else {
                bail!("RunEnvironmentInfo: environment keys and values must be strings");
            };
            budget.node().map_err(anyhow::Error::msg)?;
            budget.string(key).map_err(anyhow::Error::msg)?;
            budget.string(value).map_err(anyhow::Error::msg)?;
            budget
                .bytes(std::mem::size_of::<usize>())
                .map_err(anyhow::Error::msg)?;
        }
    }
    for value in inherited {
        let value = value.unpack_str().ok_or_else(|| {
            anyhow!("RunEnvironmentInfo: inherited_environment entries must be strings")
        })?;
        // Duplicates are neither deduplicated nor discounted.
        budget.string(value).map_err(anyhow::Error::msg)?;
    }
    let environment: Vec<_> = environment
        .iter()
        .flat_map(|dict| dict.iter())
        .map(|(key, value)| {
            (
                key.unpack_str().unwrap().to_owned(),
                value.unpack_str().unwrap().to_owned(),
            )
        })
        .collect();
    let inherited_environment = inherited
        .iter()
        .map(|value| value.unpack_str().unwrap().to_owned())
        .collect();
    let mut canonical: Vec<_> = (0..environment.len()).collect();
    canonical.sort_unstable_by(|&a, &b| environment[a].0.cmp(&environment[b].0));
    Ok(InfoData {
        environment,
        inherited_environment,
        canonical,
        error_on_non_executable,
    })
}

impl RunEnvironmentInfo {
    fn environment<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc(AllocDict(
            self.0
                .environment
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        ))
    }

    fn inherited_environment<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc(AllocList(
            self.0.inherited_environment.iter().map(String::as_str),
        ))
    }
}

impl fmt::Display for RunEnvironmentInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Stream the bounded owned snapshot without a temporary heap/copy.
        // Pinned starlark-rust primitive repr escapes Unicode unlike Bazel, so
        // this provider's representation must not delegate to dict/list repr.
        f.write_str("struct(environment = {")?;
        for (index, (key, value)) in self.0.environment.iter().enumerate() {
            if index != 0 {
                f.write_str(", ")?;
            }
            write_quoted(f, key)?;
            f.write_str(": ")?;
            write_quoted(f, value)?;
        }
        f.write_str("}, inherited_environment = [")?;
        for (index, value) in self.0.inherited_environment.iter().enumerate() {
            if index != 0 {
                f.write_str(", ")?;
            }
            write_quoted(f, value)?;
        }
        f.write_str("])")
    }
}

/// Bazel 9.3 Printer.java appendQuoted/escapeCharacter: quote/backslash and
/// CR/LF/TAB have short escapes; remaining ASCII bytes below 32 use lowercase
/// hex. DEL and all non-ASCII UTF-8 (including non-ASCII controls) stay literal.
/// This is deliberately provider-local, not a change to primitive Starlark repr.
fn write_quoted(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    f.write_str("\"")?;
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        if byte >= 32 && byte != b'"' && byte != b'\\' {
            continue;
        }
        // Escapes are ASCII only, so both slice endpoints are UTF-8 boundaries.
        f.write_str(&value[start..index])?;
        match byte {
            b'"' => f.write_str("\\\"")?,
            b'\\' => f.write_str("\\\\")?,
            b'\r' => f.write_str("\\r")?,
            b'\n' => f.write_str("\\n")?,
            b'\t' => f.write_str("\\t")?,
            _ => write!(f, "\\x{byte:02x}")?,
        }
        start = index + 1;
    }
    f.write_str(&value[start..])?;
    f.write_str("\"")
}

#[starlark_value(type = "RunEnvironmentInfo")]
impl<'v> StarlarkValue<'v> for RunEnvironmentInfo {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match name {
            "environment" => Some(self.environment(heap)),
            "inherited_environment" => Some(self.inherited_environment(heap)),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["environment".to_owned(), "inherited_environment".to_owned()]
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some_and(|other| {
            self.0.inherited_environment == other.0.inherited_environment
                && self.0.environment.len() == other.0.environment.len()
                && self
                    .0
                    .canonical
                    .iter()
                    .zip(&other.0.canonical)
                    .all(|(&a, &b)| self.0.environment[a] == other.0.environment[b])
        }))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        self.0.environment.len().hash(hasher);
        for &index in &self.0.canonical {
            self.0.environment[index].hash(hasher);
        }
        self.0.inherited_environment.hash(hasher);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{FrozenModule, Module};
    use starlark::syntax::{AstModule, Dialect};

    fn eval(module: &Module, source: &str) -> anyhow::Result<()> {
        let ast = AstModule::parse("run_environment.bzl", source.to_owned(), &Dialect::Standard)
            .map_err(|error| anyhow!("{error}"))?;
        Evaluator::new(module)
            .eval_module(ast, &crate::package::bzl_globals())
            .map_err(|error| anyhow!("{error}"))?;
        Ok(())
    }

    fn evaluate(source: &str) -> anyhow::Result<FrozenModule> {
        Module::with_temp_heap(|module| {
            eval(&module, &format!("R = RunEnvironmentInfo\n{source}"))?;
            module.freeze().map_err(|error| anyhow!("{error:?}"))
        })
    }

    #[test]
    fn run_environment_constructor_binding_types_and_immutable_fields() {
        evaluate(r#"
(type(R) == "Provider" and repr(R) == "<function RunEnvironmentInfo>" and dir(R) == []) or fail("constructor")
(R().environment == {} and R().inherited_environment == []) or fail("defaults")
(R({"x": "y"}).environment == {"x": "y"}) or fail("positional")
(R({}, ["A"]) == R(environment={}, inherited_environment=("A",))) or fail("binding")
(R({}, inherited_environment=["A"]) == R(inherited_environment=["A"])) or fail("mixed")
(R(*[{"x": "y"}, ("A",)]) == R(**{"environment": {"x": "y"}, "inherited_environment": ["A"]})) or fail("expanded")
(type(R()) == "RunEnvironmentInfo" and dir(R()) == ["environment", "inherited_environment"]) or fail("fields")
(not hasattr(R(), "error_on_non_executable") and not hasattr(R(), "unknown")) or fail("hidden fields")
"#).unwrap();
        for source in [
            "R(None)",
            "R(environment=None)",
            "R([], [])",
            "R('x')",
            "R({1: 'v'})",
            "R({'k': 1})",
            "R({'k': None})",
            "R({'k': []})",
            "R(inherited_environment=None)",
            "R({}, 'A')",
            "R({}, {})",
            "R({}, [1])",
            "R({}, (None,))",
            "R({}, [['A']])",
            "R({}, [], [])",
            "R({}, environment={})",
            "R({}, [], inherited_environment=[])",
            "R(unknown={})",
            "R(error_on_non_executable=False)",
            "R(error_on_non_executable=True)",
            "R(environment={}, **{'environment': {}})",
            "R(**{1: {}})",
            "R().environment = {}",
            "R().inherited_environment = []",
            "R().unknown",
        ] {
            assert!(evaluate(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn run_environment_snapshot_and_fresh_mutable_projections() {
        evaluate(
            r#"
env = {"z": "first", "a": "second", "": ""}
names = ["B", "A", "B", ""]
x = R(env, names)
env["z"] = "changed"
env["late"] = "late"
names.append("late")
first = x.environment
first.pop("a")
first["z"] = "local"
second = x.environment
second["a"] = "local"
inherited = x.inherited_environment
inherited.append("local")
(x.environment == {"z": "first", "a": "second", "": ""}) or fail("snapshot")
(list(x.environment) == ["z", "a", ""]) or fail("insertion order")
(x.inherited_environment == ["B", "A", "B", ""]) or fail("duplicates/order")
(env["z"] == "changed" and names[-1] == "late") or fail("caller mutable")
empty = R()
copy = empty.environment
copy["x"] = "v"
copy_list = empty.inherited_environment
copy_list.append("x")
(empty.environment == {} and empty.inherited_environment == []) or fail("empty isolation")
"#,
        )
        .unwrap();
    }

    #[test]
    fn run_environment_structural_equality_hash_and_native_repr() {
        let frozen = evaluate(r#"
x = R({"z": "雪\x00\n\"'\\", "a": "", "": "é"}, ["B", "A", "B", ""])
y = R({"": "é", "a": "", "z": "雪\x00\n\"'\\"}, ("B", "A", "B", ""))
(x == y and x == x) or fail("map order independent")
keys = {x: "first"}
keys[y] = "replacement"
(len(keys) == 1 and keys[x] == "replacement") or fail("equal hash")
(x != R(x.environment, ["A", "B", "B", ""])) or fail("list order")
(x != R(x.environment, ["B", "A", ""])) or fail("list duplicates")
(x != R({"z": "other", "a": "", "": "é"}, x.inherited_environment)) or fail("map values")
(x != R({"z": "雪\x00\n\"'\\", "other": "", "": "é"}, x.inherited_environment)) or fail("map keys")
(x != R({}, x.inherited_environment) and R() != R({}, [""])) or fail("length")
(x != R and x != struct(environment=x.environment, inherited_environment=x.inherited_environment)) or fail("native class")
(x != platform_common.TemplateVariableInfo(x.environment)) or fail("template class")
(repr(R()) == "struct(environment = {}, inherited_environment = [])") or fail("empty repr")
"#).unwrap();
        assert_eq!(
            frozen.get("x").unwrap().value().to_repr(),
            r#"struct(environment = {"z": "雪\x00\n\"'\\", "a": "", "": "é"}, inherited_environment = ["B", "A", "B", ""])"#,
        );
    }

    #[test]
    fn run_environment_repr_matches_genuine_case02_exact_bytes() {
        use sha2::Digest;

        let frozen = evaluate(
            r#"x = R({"Z":"last","A":"first","":"","é":"☃","nul":"a\000b"},("Z","Z","", "☃"))"#,
        )
        .unwrap();
        let actual = frozen.get("x").unwrap().value().to_repr();
        let expected = r#"struct(environment = {"Z": "last", "A": "first", "": "", "é": "☃", "nul": "a\x00b"}, inherited_environment = ["Z", "Z", "", "☃"])"#;
        assert_eq!(actual.as_bytes(), expected.as_bytes());
        // Pinned genuine case02 output, not this interpreter's primitive repr.
        assert_eq!(
            format!("{:x}", sha2::Sha256::digest(actual.as_bytes())),
            "312769f655ab40b0862f20dba494806d498c51351017e92dce87deab21cbd70e"
        );
    }

    #[test]
    fn run_environment_repr_quotes_ascii_controls_and_preserves_other_utf8() {
        Module::with_temp_heap(|module| {
            let input: String = (0_u8..32)
                .map(char::from)
                .chain("\"\\'\u{7f}\u{80}\u{85}é☃雪🦀".chars())
                .collect();
            // Printer.java escapes c < 32, not DEL or non-ASCII controls.
            let quoted = concat!(
                "\"\\x00\\x01\\x02\\x03\\x04\\x05\\x06\\x07\\x08\\t\\n\\x0b\\x0c\\r\\x0e\\x0f",
                "\\x10\\x11\\x12\\x13\\x14\\x15\\x16\\x17\\x18\\x19\\x1a\\x1b\\x1c\\x1d\\x1e\\x1f",
                "\\\"\\\\'\u{7f}\u{80}\u{85}é☃雪🦀\"",
            );
            let environment = module
                .heap()
                .alloc(AllocDict([(input.as_str(), input.as_str())]));
            let inherited = module
                .heap()
                .alloc(AllocList([input.as_str(), input.as_str()]));
            let data = snapshot(Some(environment), Some(inherited), true).unwrap();
            let info = module.heap().alloc(RunEnvironmentInfo(Arc::new(data)));
            let expected = format!(
                "struct(environment = {{{quoted}: {quoted}}}, inherited_environment = [{quoted}, {quoted}])"
            );
            assert_eq!(info.to_repr().as_bytes(), expected.as_bytes());
        });
    }

    #[test]
    fn run_environment_frozen_inputs_snapshots_aliases_and_clones() {
        let original = evaluate("env = {'雪': 'v', 'a': ''}\nnames = ['B', 'A', 'B']\nx = R(env, names)\nsaved_env = x.environment\nsaved_names = x.inherited_environment\nAlias = R\nnested = [R]").unwrap();
        let reexport = Module::with_temp_heap(|module| {
            module.import_public_symbols(&original);
            eval(&module, "Reexport = nested[0]").unwrap();
            module.freeze().unwrap()
        });
        let x = original.get("x").unwrap();
        let alias = reexport.get("Reexport").unwrap();
        let env = original.get("env").unwrap();
        let names = original.get("names").unwrap();
        let saved_env = original.get("saved_env").unwrap();
        let saved_names = original.get("saved_names").unwrap();
        drop(original);
        drop(reexport);
        Module::with_temp_heap(|module| {
            for (name, value) in [
                ("x", &x),
                ("Alias", &alias),
                ("env", &env),
                ("names", &names),
                ("saved_env", &saved_env),
                ("saved_names", &saved_names),
            ] {
                module.set(name, module.heap().access_owned_frozen_value(value));
            }
            module.set(
                "cloned",
                module
                    .heap()
                    .alloc(RunEnvironmentInfo::from_value(x.value()).unwrap().clone()),
            );
            eval(
                &module,
                r#"
R = RunEnvironmentInfo
(Alias == R and {Alias: 1}[R] == 1) or fail("alias identity")
(R(env, names) == x and x == cloned and {x: 1}[cloned] == 1) or fail("frozen inputs")
copy = x.environment
copy["雪"] = "local"
copy_names = x.inherited_environment
copy_names.append("local")
(x.environment == env and x.inherited_environment == names) or fail("frozen receiver projection")
"#,
            )
            .unwrap();
            assert!(eval(&module, "saved_env['local'] = 'v'").is_err());
            assert!(eval(&module, "saved_names.append('local')").is_err());
        });
        assert!(alias.value().parameters_spec().is_none());
    }

    #[test]
    fn run_environment_budget_preflights_combined_borrowed_inputs() {
        Module::with_temp_heap(|module| {
            let large = "x".repeat(4 * 1024 * 1024);
            let large_value = module.heap().alloc(large.as_str());
            let env = module.heap().alloc(AllocDict([("k", large_value)]));
            let inherited = module.heap().alloc(AllocList([large_value]));
            // Either field alone fits, but together exceed the weighted limit.
            assert!(snapshot(Some(env), None, true).is_ok());
            assert!(snapshot(None, Some(inherited), true).is_ok());
            let error = snapshot(Some(env), Some(inherited), true)
                .unwrap_err()
                .to_string();
            assert!(error.contains("weighted byte limit"), "{error}");
            let repeated = module.heap().alloc(AllocList([large_value, large_value]));
            assert!(
                snapshot(None, Some(repeated), true)
                    .unwrap_err()
                    .to_string()
                    .contains("weighted byte limit")
            );
            let huge = module.heap().alloc("x".repeat(8 * 1024 * 1024));
            let huge_env = module.heap().alloc(AllocDict([(huge, "v")]));
            assert!(
                snapshot(Some(huge_env), None, true)
                    .unwrap_err()
                    .to_string()
                    .contains("weighted byte limit")
            );
            let repeated = module
                .heap()
                .alloc(AllocList(std::iter::repeat_n("", 100_001)));
            assert!(
                snapshot(None, Some(repeated), true)
                    .unwrap_err()
                    .to_string()
                    .contains("node limit")
            );
            module.set("huge_env", huge_env);
            let error = eval(&module, "RunEnvironmentInfo(huge_env)")
                .unwrap_err()
                .to_string();
            assert!(error.contains("weighted byte limit"), "{error}");
        });
    }
}
