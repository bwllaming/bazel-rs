//! Owned, bounded call-time snapshots of ordinary extension repository declarations.
//! Only the callee is rooted in the callback module: argument containers must not
//! remain live until that module freezes, because the callback can mutate them.

use anyhow::{anyhow, bail};
use starlark::environment::FrozenModule;
use starlark::eval::{Arguments, Evaluator};
use starlark::values::Value;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::tuple::TupleRef;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use crate::repository_budget::HostBudget;
use crate::repository_rule::{self, RuleDefinition};

const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 100_000;
const MAX_CALL_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_CALLS: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum RecordedValue {
    None,
    Bool(bool),
    Int(i32),
    String(String),
    Label(String),
    List(Vec<RecordedValue>),
    Dict(Vec<(String, RecordedValue)>),
    /// A dict with at least one Label key, in insertion order; each key is a
    /// `String` or `Label` snapshot. Only `attr.label_keyed_string_dict`
    /// accepts it (BuildType.LabelKeyedDictType, packages/BuildType.java:436-490).
    LabelKeyedDict(Vec<(RecordedValue, RecordedValue)>),
}

#[derive(Debug)]
pub struct RecordedRepository {
    pub definition: RuleDefinition,
    pub attributes: BTreeMap<String, RecordedValue>,
    pub name: String,
}

#[derive(Debug)]
struct PendingCall {
    slot: String,
    attributes: BTreeMap<String, RecordedValue>,
    name: String,
}

#[derive(Debug, Default)]
struct State {
    calls: Vec<PendingCall>,
    names: BTreeSet<String>,
    bytes: usize,
    host: HostBudget,
}

#[derive(Debug, Default)]
pub struct CallRecorder {
    state: RefCell<State>,
}

impl CallRecorder {
    /// The caller must first establish the ordinary-extension execution phase.
    /// Schema binding is deliberately deferred until the whole callback succeeds.
    pub fn record<'v>(
        &self,
        me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        repository_rule::validate_invokable_value(me)?;
        args.no_positional_args(eval.heap())
            .map_err(|e| anyhow!("{e}"))?;
        let named = args.names_map().map_err(|e| anyhow!("{e}"))?;
        self.record_values(
            me,
            named
                .iter()
                .map(|(key, value)| (key.as_str(), *value))
                .collect(),
            eval,
        )
    }

    fn record_values<'v>(
        &self,
        me: Value<'v>,
        raw: BTreeMap<&str, Value<'v>>,
        eval: &Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        repository_rule::validate_invokable_value(me)?;
        let name = raw
            .get("name")
            .and_then(|value| value.unpack_str())
            .ok_or_else(|| anyhow!("repository rule call requires a string name"))?;
        crate::bzlmod::validate_repo_name(name)?;
        let mut state = self.state.borrow_mut();
        if state.names.contains(name) {
            bail!("duplicate generated repository name {name:?}");
        }
        if state.calls.len() >= MAX_CALLS {
            bail!("ordinary extension exceeds {MAX_CALLS} repository calls");
        }
        // Schema defaults are expanded after the callback. Reserve their host
        // copies now, before recording/rooting this callee, even for name-only
        // calls. No eager schema validation or label coercion takes place.
        repository_rule::charge_schema(me, &mut state.host).map_err(anyhow::Error::msg)?;
        state.host.node().map_err(anyhow::Error::msg)?;
        let mut budget = SnapshotBudget::default();
        let mut attributes = BTreeMap::new();
        for (key, value) in raw {
            budget.key(key, 1)?;
            state.host.string(key).map_err(anyhow::Error::msg)?;
            let value = snapshot(value, 1, &mut budget, &mut state.host)?;
            attributes.insert(key.to_owned(), value);
        }
        if budget.bytes > MAX_TOTAL_BYTES - state.bytes {
            bail!("ordinary extension repository snapshots exceed {MAX_TOTAL_BYTES} total bytes");
        }
        // '$' is not a Starlark identifier: user assignments cannot overwrite
        // these roots, even when the rule itself came from a private load.
        let slot = format!("__extension_repo_call${}", state.calls.len());
        eval.module().set(&slot, me);
        state.bytes += budget.bytes;
        state.names.insert(name.to_owned());
        state.calls.push(PendingCall {
            slot,
            attributes,
            name: name.to_owned(),
        });
        Ok(Value::new_none())
    }

    /// Call only after evaluation and return-protocol validation succeeded and
    /// the callback module froze. Each definition retains its own frozen owner.
    pub fn finish(self, frozen: &FrozenModule) -> Result<Vec<RecordedRepository>, String> {
        self.state
            .into_inner()
            .calls
            .into_iter()
            .map(|call| {
                let (callee, _) = frozen
                    .get_any_visibility(&call.slot)
                    .map_err(|e| format!("recorded repository callee {}: {e}", call.name))?;
                Ok(RecordedRepository {
                    definition: repository_rule::unpack(&callee)?,
                    attributes: call.attributes,
                    name: call.name,
                })
            })
            .collect()
    }
}

#[derive(Default)]
struct SnapshotBudget {
    nodes: usize,
    bytes: usize,
}

impl SnapshotBudget {
    fn node(&mut self, depth: usize) -> anyhow::Result<()> {
        if depth > MAX_DEPTH {
            bail!("repository snapshot exceeds maximum depth {MAX_DEPTH} (possibly cyclic)");
        }
        if self.nodes >= MAX_NODES {
            bail!("repository snapshot exceeds {MAX_NODES} nodes");
        }
        self.nodes += 1;
        // Charge one discriminant byte even for empty containers and None.
        self.add_bytes(1)
    }

    fn add_bytes(&mut self, bytes: usize) -> anyhow::Result<()> {
        if bytes > MAX_CALL_BYTES - self.bytes {
            bail!("repository snapshot exceeds {MAX_CALL_BYTES} bytes per call");
        }
        self.bytes += bytes;
        Ok(())
    }

    fn string(&mut self, text: &str) -> anyhow::Result<()> {
        self.add_bytes(text.len())?;
        if !repository_rule::is_latin1(text) {
            bail!(
                "repository snapshot strings above U+00FF are unsupported \
                 (Bazel strings are Latin-1 byte strings)"
            );
        }
        Ok(())
    }

    fn key(&mut self, text: &str, depth: usize) -> anyhow::Result<()> {
        self.node(depth)?;
        self.string(text)
    }
}

fn snapshot(
    value: Value<'_>,
    depth: usize,
    budget: &mut SnapshotBudget,
    host: &mut HostBudget,
) -> anyhow::Result<RecordedValue> {
    budget.node(depth)?;
    host.node().map_err(anyhow::Error::msg)?;
    Ok(if value.is_none() {
        RecordedValue::None
    } else if let Some(value) = value.unpack_bool() {
        RecordedValue::Bool(value)
    } else if let Some(value) = value.unpack_i32() {
        budget.add_bytes(4)?;
        RecordedValue::Int(value)
    } else if let Some(value) = value.unpack_str() {
        budget.string(value)?;
        host.bytes(value.len()).map_err(anyhow::Error::msg)?;
        RecordedValue::String(value.to_owned())
    } else if let Some((repository, package, name)) = crate::starlark_types::label_parts(value) {
        let len = repository
            .len()
            .saturating_add(package.len())
            .saturating_add(name.len())
            .saturating_add(if repository.is_empty() { 3 } else { 5 });
        budget.add_bytes(len)?;
        host.bytes(len).map_err(anyhow::Error::msg)?;
        let label = if repository.is_empty() {
            format!("//{package}:{name}")
        } else {
            format!("@@{repository}//{package}:{name}")
        };
        RecordedValue::Label(label)
    } else if let Some(list) = ListRef::from_value(value) {
        let mut values = Vec::new();
        for item in list.iter() {
            values.push(snapshot(item, depth + 1, budget, host)?);
        }
        RecordedValue::List(values)
    } else if let Some(tuple) = TupleRef::from_value(value) {
        // Attribute list data accepts tuples, not arbitrary Starlark iterables.
        let mut values = Vec::new();
        for item in tuple.iter() {
            values.push(snapshot(item, depth + 1, budget, host)?);
        }
        RecordedValue::List(values)
    } else if let Some(dict) = DictRef::from_value(value) {
        // Label keys are meaningful only for attr.label_keyed_string_dict
        // (BuildType.LabelKeyedDictType); string-keyed snapshots keep the
        // pre-existing shape.
        if dict
            .iter()
            .any(|(key, _)| crate::starlark_types::label_parts(key).is_some())
        {
            let mut values = Vec::new();
            for (key, item) in dict.iter() {
                if key.unpack_str().is_none() && crate::starlark_types::label_parts(key).is_none() {
                    bail!("repository snapshot dict keys must be strings or Labels");
                }
                let key = snapshot(key, depth + 1, budget, host)?;
                let item = snapshot(item, depth + 1, budget, host)?;
                values.push((key, item));
            }
            return Ok(RecordedValue::LabelKeyedDict(values));
        }
        let mut values = Vec::new();
        for (key, item) in dict.iter() {
            let key = key.unpack_str().ok_or_else(|| {
                anyhow!("repository snapshot dict keys must be strings or Labels")
            })?;
            budget.key(key, depth + 1)?;
            host.string(key).map_err(anyhow::Error::msg)?;
            let item = snapshot(item, depth + 1, budget, host)?;
            values.push((key.to_owned(), item));
        }
        RecordedValue::Dict(values)
    } else if value.get_type() == "int" {
        bail!(
            "repository snapshot integers outside the supported 32-bit int range are unsupported"
        );
    } else {
        bail!(
            "unsupported repository snapshot value of type {}",
            value.get_type()
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{Globals, GlobalsBuilder, Module};
    use starlark::starlark_module;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::{Heap, OwnedFrozenValue};

    #[allow(unsafe_code)]
    mod values {
        use super::*;

        #[derive(starlark::values::ProvidesStaticType)]
        pub(super) struct TestContext {
            pub calls: CallRecorder,
            pub rule: OwnedFrozenValue,
        }
    }
    use values::TestContext;

    #[starlark_module]
    fn capture_globals(builder: &mut GlobalsBuilder) {
        fn capture<'v>(
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<Value<'v>> {
            let context = eval.extra.unwrap().downcast_ref::<TestContext>().unwrap();
            let rule = eval.heap().access_owned_frozen_value(&context.rule);
            context.calls.record(rule, args, eval)
        }

        fn main_label<'v>(heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
            crate::starlark_types::alloc_label(heap, "//original:data")
        }
    }

    fn globals() -> Globals {
        GlobalsBuilder::standard().with(capture_globals).build()
    }

    fn ast(source: &str) -> AstModule {
        AstModule::parse("capture.star", source.to_owned(), &Dialect::Standard).unwrap()
    }

    fn rule() -> OwnedFrozenValue {
        Module::with_temp_heap(|module| {
            let context = crate::loading::ExtensionContext {
                byte_strings: false,
                label: crate::label::Label::parse_canonical("//defs:repos.bzl").unwrap(),
                repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                    std::path::PathBuf::from("."),
                )),
            };
            let globals = GlobalsBuilder::standard()
                .with(repository_rule::globals)
                .build();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(
                    ast("def _private(x):\n    return x + 1\ndef _impl(ctx):\n    return _private(ctx)\nR = repository_rule(_impl)"),
                    &globals,
                )
                .unwrap();
            }
            module.freeze().unwrap().get("R").unwrap()
        })
    }

    fn capture_with(calls: CallRecorder, source: &str) -> Result<Vec<RecordedRepository>, String> {
        Module::with_temp_heap(|module| {
            let context = TestContext {
                calls,
                rule: rule(),
            };
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast(source), &globals())
                    .map_err(|e| e.to_string())?;
            }
            let frozen = module.freeze().map_err(|e| format!("{e:?}"))?;
            // Discard even the test's explicit callee owner before unpacking.
            drop(context.rule);
            context.calls.finish(&frozen)
        })
    }

    fn capture(source: &str) -> Result<Vec<RecordedRepository>, String> {
        capture_with(CallRecorder::default(), source)
    }

    #[test]
    fn snapshots_nested_mutable_values_at_each_call_not_at_freeze() {
        let calls = capture(
            "words = ['before']\nnested = {'k': [words]}\ncapture(name='first', words=words, nested=nested) == None or fail('return')\nwords.append('after')\nnested['new'] = True\ncapture(name='second', words=words, nested=nested)\nwords.append('last')",
        ).unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        let before = RecordedValue::List(vec![RecordedValue::String("before".to_owned())]);
        assert_eq!(calls[0].attributes["words"], before);
        assert_eq!(
            calls[0].attributes["nested"],
            RecordedValue::Dict(vec![(
                "k".to_owned(),
                RecordedValue::List(vec![before.clone()])
            )])
        );
        assert_eq!(
            calls[1].attributes["words"],
            RecordedValue::List(vec![
                RecordedValue::String("before".to_owned()),
                RecordedValue::String("after".to_owned()),
            ])
        );
        // The snapshot is supported independently of the eventual schema.
        assert!(
            repository_rule::bind_recorded_attributes(
                &calls[0].definition,
                &calls[0].attributes,
                "canonical",
                crate::label::Label::parse_canonical,
            )
            .unwrap_err()
            .contains("unknown repository attribute")
        );
    }

    #[test]
    fn ordered_dicts_and_nested_tuples_snapshot_each_call_with_typed_labels() {
        let calls = capture(
            "words = ['before']\nnested = {'z': (words, main_label(), '//original:data'), 'a': ()}\ncapture(name='first', z=(nested,), a=[(1, 2)])\nwords.append('between')\nnested['m'] = ('new',)\ncapture(name='second', z=(nested,), a=[(1, 2)])\nwords.append('after')",
        ).unwrap();
        assert_eq!(
            calls[0]
                .attributes
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["a", "name", "z"]
        );
        let expected = |words: &[&str], extra: bool| {
            let mut dict = vec![
                (
                    "z".to_owned(),
                    RecordedValue::List(vec![
                        RecordedValue::List(
                            words
                                .iter()
                                .map(|s| RecordedValue::String((*s).to_owned()))
                                .collect(),
                        ),
                        RecordedValue::Label("//original:data".to_owned()),
                        RecordedValue::String("//original:data".to_owned()),
                    ]),
                ),
                ("a".to_owned(), RecordedValue::List(vec![])),
            ];
            if extra {
                dict.push((
                    "m".to_owned(),
                    RecordedValue::List(vec![RecordedValue::String("new".to_owned())]),
                ));
            }
            RecordedValue::List(vec![RecordedValue::Dict(dict)])
        };
        assert_eq!(calls[0].attributes["z"], expected(&["before"], false));
        assert_eq!(
            calls[1].attributes["z"],
            expected(&["before", "between"], true)
        );
        assert_eq!(
            calls[0].attributes["a"],
            RecordedValue::List(vec![RecordedValue::List(vec![
                RecordedValue::Int(1),
                RecordedValue::Int(2)
            ])])
        );
    }

    #[test]
    fn hidden_callee_roots_retain_private_implementation_without_executing_it() {
        // Calling this implementation with repository ctx would fail its +1.
        // Recording does not call it, and its private helper outlives all modules.
        let mut calls = capture("capture(name='made')").unwrap();
        let call = calls.pop().unwrap();
        assert_eq!(call.definition.identity, "//defs:repos.bzl%R");
        Module::with_temp_heap(|module| {
            let function = module
                .heap()
                .access_owned_frozen_value(&call.definition.implementation);
            let result = Evaluator::new(&module)
                .eval_function(function, &[module.heap().alloc(41)], &[])
                .unwrap();
            assert_eq!(result.unpack_i32(), Some(42));
        });
    }

    #[test]
    fn preserves_scalar_types_and_genuine_labels() {
        let calls = capture("capture(name='made', values=[None, True, False, -2147483648, 2147483647, '//original:data', main_label()])").unwrap();
        assert_eq!(
            calls[0].attributes["values"],
            RecordedValue::List(vec![
                RecordedValue::None,
                RecordedValue::Bool(true),
                RecordedValue::Bool(false),
                RecordedValue::Int(i32::MIN),
                RecordedValue::Int(i32::MAX),
                RecordedValue::String("//original:data".to_owned()),
                RecordedValue::Label("//original:data".to_owned()),
            ])
        );
    }

    #[test]
    fn validates_keyword_name_and_duplicates_before_snapshot() {
        for (source, message) in [
            ("capture('made')", "positional"),
            ("capture()", "string name"),
            ("capture(name=None)", "string name"),
            ("capture(name=main_label())", "string name"),
            ("capture(name='')", "invalid repository name"),
            ("capture(name='a+b')", "invalid repository name"),
            (
                "capture(name='made')\ncapture(name='made', bad=capture)",
                "duplicate generated repository",
            ),
        ] {
            let error = capture(source).unwrap_err();
            assert!(error.contains(message), "{source}: {error}");
        }
    }

    #[test]
    fn unsupported_snapshots_are_explicit_and_ascii_checked_recursively() {
        for (expression, message) in [
            ("2147483648", "32-bit int"),
            ("-2147483649", "32-bit int"),
            ("1267650600228229401496703205376", "32-bit int"),
            ("1.5", "type float"),
            ("capture", "unsupported repository snapshot"),
            ("'Ā'", "above U+00FF"),
            ("{'k': ['Ā']}", "above U+00FF"),
            ("{'k': ('Ā',)}", "above U+00FF"),
            ("{'Ā': 'ok'}", "above U+00FF"),
            ("{1: 'ok'}", "dict keys"),
            (
                "{main_label(): 'ok', 1: 'no'}",
                "dict keys must be strings or Labels",
            ),
        ] {
            let source = format!("capture(name='made', value={expression})");
            let error = capture(&source).unwrap_err();
            assert!(error.contains(message), "{source}: {error}");
        }
        let error = capture("capture(**{'name': 'made', 'Ā': None})").unwrap_err();
        assert!(error.contains("above U+00FF"), "{error}");
        // Latin-1 runtime byte data (U+0000..U+00FF) is representable.
        capture("capture(name='made', value={'k' + chr(255): [chr(0) + chr(233) + chr(255)]})")
            .unwrap();
        // Label keys (attr.label_keyed_string_dict) keep their identity, in
        // insertion order, next to string keys.
        let calls =
            capture("capture(name='made', value={'//:s': 'a', main_label(): 'b'})").unwrap();
        assert_eq!(
            calls[0].attributes["value"],
            RecordedValue::LabelKeyedDict(vec![
                (
                    RecordedValue::String("//:s".to_owned()),
                    RecordedValue::String("a".to_owned())
                ),
                (
                    RecordedValue::Label("//original:data".to_owned()),
                    RecordedValue::String("b".to_owned())
                ),
            ])
        );
        // String-keyed snapshots keep their pre-existing shape.
        assert!(matches!(
            capture("capture(name='made', value={'k': 'v'})").unwrap()[0].attributes["value"],
            RecordedValue::Dict(_)
        ));
    }

    #[test]
    fn depth_cycles_and_node_limits_fail_without_unbounded_recursion() {
        let deep = format!(
            "{}None{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        );
        for source in [
            format!("capture(name='made', value={deep})"),
            "x=[]\nx.append(x)\ncapture(name='made', value=x)".to_owned(),
            "x={}\nx['self']=x\ncapture(name='made', value=x)".to_owned(),
            "x=[]\nt=(x,)\nx.append(t)\ncapture(name='made', value=t)".to_owned(),
        ] {
            let error = capture(&source).unwrap_err();
            assert!(error.contains("maximum depth"), "{error}");
        }
        let error =
            capture(&format!("capture(name='made', value=[None] * {MAX_NODES})")).unwrap_err();
        assert!(error.contains("nodes"), "{error}");
    }

    #[test]
    fn call_and_total_byte_limits_reject_before_publication() {
        let error = capture(&format!(
            "capture(name='made', value='x' * {MAX_CALL_BYTES})"
        ))
        .unwrap_err();
        assert!(error.contains("bytes per call"), "{error}");
        let calls = CallRecorder::default();
        calls.state.borrow_mut().bytes = MAX_TOTAL_BYTES - 1;
        let error = capture_with(calls, "capture(name='made')").unwrap_err();
        assert!(error.contains("total bytes"), "{error}");
        let error = capture(&format!(
            "[capture(name='r' + str(i)) for i in range({})]",
            MAX_CALLS + 1
        ))
        .unwrap_err();
        assert!(error.contains("10000 repository calls"), "{error}");
    }

    #[test]
    fn tuple_and_dict_payloads_are_charged_before_copying() {
        for value in [
            format!("('x' * {MAX_CALL_BYTES},)"),
            format!("{{'x' * {MAX_CALL_BYTES}: ()}}"),
        ] {
            let error = capture(&format!("capture(name='made', value={value})")).unwrap_err();
            assert!(error.contains("bytes per call"), "{error}");
        }
        let error = capture(&format!(
            "capture(name='made', value=(None,) * {MAX_NODES})"
        ))
        .unwrap_err();
        assert!(error.contains("nodes"), "{error}");
        // The weighted host budget is tighter than the raw snapshot byte cap.
        let error =
            capture("text='x' * (1024 * 1024)\ncapture(name='made', value={'z': (text,) * 8})")
                .unwrap_err();
        assert!(
            error.contains("host preparation weighted byte limit"),
            "{error}"
        );
    }

    #[test]
    fn budget_accepts_exact_bounds_then_rejects_next_unit() {
        let mut budget = SnapshotBudget {
            nodes: MAX_NODES - 1,
            bytes: MAX_CALL_BYTES - 1,
        };
        budget.node(MAX_DEPTH).unwrap();
        assert_eq!(budget.nodes, MAX_NODES);
        assert_eq!(budget.bytes, MAX_CALL_BYTES);
        assert!(budget.node(1).unwrap_err().to_string().contains("nodes"));
        assert!(
            budget
                .add_bytes(1)
                .unwrap_err()
                .to_string()
                .contains("bytes")
        );
        assert!(SnapshotBudget::default().node(MAX_DEPTH + 1).is_err());
    }
}
